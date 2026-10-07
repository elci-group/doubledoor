//! ACCEPT against real repositories (§14/§15): a ghost enters a detached
//! worktree only through `accept_into_workspace`, optionally restricted to
//! selected surfaces, and never over the actor's uncommitted edits.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use doubledoor_core::{
    ActorId, CanonicalState, DoorSession, GitOid, IntentDescriptor, PolicySnapshot, SurfaceId,
};
use doubledoor_ghost::{accept_into_workspace, generate, GhostDiff, GhostError, GhostStore};
use doubledoor_git::Git;

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

fn file_id(path: &str) -> SurfaceId {
    SurfaceId::new(format!("file:{path}")).unwrap()
}

/// A repo at base commit A (router.rs, retry.rs, config.rs, old.rs), a
/// detached worktree at A, and one canonical ghost commit on top that edits
/// router/retry/config and deletes old.rs.
struct Scenario {
    dir: tempfile::TempDir,
    ghost: GhostDiff,
}

impl Scenario {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        raw(root, &["init", "-q"]);
        raw(root, &["config", "user.email", "test@doubledoor.dev"]);
        raw(root, &["config", "user.name", "Doubledoor Test"]);
        raw(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join(".git/info/exclude"), "ws\n.doubledoor\n").unwrap();
        for (f, c) in [
            ("router.rs", "fn dispatch() -> Response {}\n"),
            ("retry.rs", "// none\n"),
            ("config.rs", "struct RouterConfig;\n"),
            ("old.rs", "legacy\n"),
        ] {
            fs::write(root.join(f), c).unwrap();
        }
        raw(root, &["add", "-A"]);
        raw(root, &["commit", "-qm", "A"]);
        let base = GitOid::parse(&raw(root, &["rev-parse", "HEAD"])).unwrap();
        raw(
            root,
            &["worktree", "add", "-q", "--detach", "ws", base.as_str()],
        );

        fs::write(root.join("router.rs"), "fn dispatch() -> RetryOutcome {}\n").unwrap();
        fs::write(root.join("retry.rs"), "pub fn retry() {}\n").unwrap();
        fs::write(
            root.join("config.rs"),
            "struct RouterConfig { retries: u8 }\n",
        )
        .unwrap();
        fs::remove_file(root.join("old.rs")).unwrap();
        raw(root, &["add", "-A"]);
        raw(
            root,
            &[
                "-c",
                "user.name=Agent Alpha",
                "-c",
                "user.email=alpha@example.dev",
                "commit",
                "-qm",
                "retry semantics",
            ],
        );

        let session = DoorSession::new(
            ActorId::new("agent-beta").unwrap(),
            IntentDescriptor::new("rework dispatch"),
            CanonicalState {
                commit: base,
                tree: GitOid::parse(&"0".repeat(40)).unwrap(),
                captured_at: SystemTime::UNIX_EPOCH,
            },
            Vec::new(),
            PolicySnapshot::default(),
        );
        let git = Git::new(root.to_path_buf());
        let store = GhostStore::new(&git, &session.id).unwrap();
        let mut obs = generate(&git, &session, &store).unwrap();
        assert_eq!(obs.ghosts.len(), 1);
        Scenario {
            ghost: obs.ghosts.remove(0),
            dir,
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn ws(&self) -> PathBuf {
        self.root().join("ws")
    }
    fn git(&self) -> Git {
        Git::new(self.root().to_path_buf())
    }
    fn patch(&self) -> PathBuf {
        self.root().join(".doubledoor/accept.patch")
    }
    fn ws_file(&self, f: &str) -> Option<String> {
        fs::read_to_string(self.ws().join(f)).ok()
    }
}

#[test]
fn full_accept_imports_every_change_as_a_new_workspace_commit() {
    let sc = Scenario::new();
    let ws_head_before = raw(&sc.ws(), &["rev-parse", "HEAD"]);
    let canonical_head = raw(sc.root(), &["rev-parse", "HEAD"]);

    let out = accept_into_workspace(&sc.git(), &sc.ws(), &sc.ghost, None, &sc.patch()).unwrap();

    let commit = out.applied_commit.expect("a workspace commit");
    assert_eq!(raw(&sc.ws(), &["rev-parse", "HEAD"]), commit.as_str());
    assert_eq!(
        raw(&sc.ws(), &["rev-parse", "HEAD~1"]),
        ws_head_before,
        "the import is a new commit on the workspace's own history"
    );
    assert_ne!(
        commit.as_str(),
        sc.ghost.source_commit.as_str(),
        "the ghost commit itself is never reused (§15)"
    );
    assert_eq!(sc.ws_file("retry.rs").unwrap(), "pub fn retry() {}\n");
    assert!(sc.ws_file("old.rs").is_none(), "deletions are imported too");
    let msg = raw(&sc.ws(), &["log", "-1", "--format=%B"]);
    assert!(msg.contains(&format!("Dd-Ghost: {}", sc.ghost.id)));
    assert_eq!(
        raw(&sc.ws(), &["log", "-1", "--format=%an"]),
        "Agent Alpha",
        "authorship of the imported change is preserved"
    );
    assert!(
        sc.patch().is_file(),
        "the applied patch is kept as evidence"
    );
    assert_eq!(raw(sc.root(), &["rev-parse", "HEAD"]), canonical_head);
    assert!(raw(&sc.ws(), &["status", "--porcelain"]).is_empty());
}

#[test]
fn selective_accept_keeps_the_actors_version_of_held_surfaces() {
    let sc = Scenario::new();
    let selected = [file_id("retry.rs"), file_id("config.rs")];
    let out = accept_into_workspace(&sc.git(), &sc.ws(), &sc.ghost, Some(&selected), &sc.patch())
        .unwrap();

    assert_eq!(out.surfaces.len(), 2);
    assert_eq!(sc.ws_file("retry.rs").unwrap(), "pub fn retry() {}\n");
    assert_eq!(
        sc.ws_file("config.rs").unwrap(),
        "struct RouterConfig { retries: u8 }\n"
    );
    assert_eq!(
        sc.ws_file("router.rs").unwrap(),
        "fn dispatch() -> Response {}\n",
        "router.rs was held: the actor keeps its own dispatch()"
    );
    assert_eq!(sc.ws_file("old.rs").unwrap(), "legacy\n");
}

#[test]
fn accept_refuses_to_touch_uncommitted_actor_edits() {
    let sc = Scenario::new();
    fs::write(sc.ws().join("retry.rs"), "// my unsaved idea\n").unwrap();
    let head_before = raw(&sc.ws(), &["rev-parse", "HEAD"]);

    let err = accept_into_workspace(&sc.git(), &sc.ws(), &sc.ghost, None, &sc.patch()).unwrap_err();
    assert!(matches!(err, GhostError::WorkspaceDirty(_)), "{err}");
    assert_eq!(sc.ws_file("retry.rs").unwrap(), "// my unsaved idea\n");
    assert_eq!(raw(&sc.ws(), &["rev-parse", "HEAD"]), head_before);
}

#[test]
fn non_applying_ghost_leaves_the_workspace_untouched() {
    let sc = Scenario::new();
    // The actor committed its own rework of dispatch(): the ghost's hunk for
    // router.rs no longer applies.
    fs::write(sc.ws().join("router.rs"), "fn dispatch() -> Mine {}\n").unwrap();
    raw(&sc.ws(), &["commit", "-qam", "my dispatch"]);
    let head_before = raw(&sc.ws(), &["rev-parse", "HEAD"]);

    let err = accept_into_workspace(&sc.git(), &sc.ws(), &sc.ghost, None, &sc.patch()).unwrap_err();
    assert!(matches!(err, GhostError::DoesNotApply(_)), "{err}");
    assert_eq!(raw(&sc.ws(), &["rev-parse", "HEAD"]), head_before);
    assert!(raw(&sc.ws(), &["status", "--porcelain"]).is_empty());
    assert_eq!(sc.ws_file("retry.rs").unwrap(), "// none\n");

    // Selecting only the surfaces that still apply succeeds.
    let ok = accept_into_workspace(
        &sc.git(),
        &sc.ws(),
        &sc.ghost,
        Some(&[file_id("retry.rs")]),
        &sc.patch(),
    )
    .unwrap();
    assert!(ok.applied_commit.is_some());
    assert_eq!(
        sc.ws_file("router.rs").unwrap(),
        "fn dispatch() -> Mine {}\n"
    );
}

#[test]
fn selecting_a_surface_the_ghost_did_not_touch_is_rejected() {
    let sc = Scenario::new();
    let err = accept_into_workspace(
        &sc.git(),
        &sc.ws(),
        &sc.ghost,
        Some(&[file_id("elsewhere.rs")]),
        &sc.patch(),
    )
    .unwrap_err();
    assert!(matches!(err, GhostError::SurfaceNotInGhost(_)), "{err}");
}

#[test]
fn regenerating_after_a_crash_does_not_duplicate_ghosts() {
    let sc = Scenario::new();
    let git = sc.git();
    // Same session id: the store already holds the ghost for the only commit.
    let mut session = DoorSession::new(
        ActorId::new("agent-beta").unwrap(),
        IntentDescriptor::new("x"),
        CanonicalState {
            commit: GitOid::parse(&raw(sc.root(), &["rev-parse", "HEAD~1"])).unwrap(),
            tree: GitOid::parse(&"0".repeat(40)).unwrap(),
            captured_at: SystemTime::UNIX_EPOCH,
        },
        Vec::new(),
        PolicySnapshot::default(),
    );
    session.id = sc.ghost.session;
    let store = GhostStore::new(&git, &session.id).unwrap();
    let again = generate(&git, &session, &store).unwrap();
    assert!(
        again.ghosts.is_empty(),
        "cursor was not advanced, but the commit is already recorded"
    );
    assert_eq!(store.list().unwrap().len(), 1);
}
