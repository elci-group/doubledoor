//! Phase II exit criterion (§46): a speculative actor receives structured
//! notification of relevant canonical mutations without having its workspace
//! altered automatically — and changes enter the workspace only through an
//! explicit, journalled `accept`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use doubledoor_core::DoorState;
use doubledoor_protocol::{DecisionView, GhostView, SessionSummary};

/// A canonical commit by agent-alpha in the user's worktree, using
/// `git add -A` as a careless actor would.
fn alpha_commits(repo: &Path, files: &[(&str, &str)], message: &str) -> String {
    // The fixture's notice files are test scaffolding, not α's work.
    fs::write(repo.join(".git/info/exclude"), "notice-*.json\n").unwrap();
    for (f, c) in files {
        fs::write(repo.join(f), c).unwrap();
    }
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            "user.name=agent-alpha",
            "-c",
            "user.email=alpha@example.dev",
            "commit",
            "-qm",
            message,
            "--trailer",
            "dd-actor: agent-alpha",
            "--trailer",
            "dd-intent: add retry semantics",
        ],
    );
    git(repo, &["rev-parse", "HEAD"])
}

fn workspace(s: &SessionSummary) -> PathBuf {
    s.workspace.clone().expect("session workspace")
}

/// Everything observable about a workspace: HEAD, tracked contents, status.
fn snapshot(ws: &Path) -> (String, String, String, String) {
    (
        git(ws, &["rev-parse", "HEAD"]),
        read(&ws.join("alpha.txt")),
        read(&ws.join("beta.txt")),
        git(ws, &["status", "--porcelain", "--untracked-files=all"]),
    )
}

fn ghosts(repo: &Path, id: &str) -> Vec<GhostView> {
    serde_json::from_str(&dd(repo, &["ghosts", id, "--format", "json"])).unwrap()
}

fn events(repo: &Path, extra: &[&str]) -> Vec<serde_json::Value> {
    let mut args = vec!["events", "--format", "json"];
    args.extend_from_slice(extra);
    dd(repo, &args)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("event JSON line"))
        .collect()
}

fn decide(repo: &Path, verb: &str, ghost: &str, extra: &[&str]) -> DecisionView {
    let mut args = vec![verb, ghost, "--format", "json"];
    args.extend_from_slice(extra);
    serde_json::from_str(&dd(repo, &args)).unwrap()
}

fn state_of(repo: &Path, id: &str) -> DoorState {
    sessions(repo)
        .into_iter()
        .find(|s| s.id.to_string() == id)
        .unwrap()
        .state
}

#[test]
fn exit_criterion_notified_without_workspace_mutation() {
    let fx = Fixture::new();
    let repo = fx.root();
    // β writes alpha.txt (its contended surface) and reads beta.txt.
    let beta = open_session_with(
        repo,
        "agent-beta",
        "alpha.txt",
        "agent-alpha",
        &["--read", "beta.txt"],
    );
    let id = beta.id.to_string();
    assert_eq!(beta.write_set, ["file:alpha.txt"]);
    assert_eq!(beta.read_set, ["file:beta.txt"]);
    let ws = workspace(&beta);
    let before = snapshot(&ws);

    // α changes β's basis (beta.txt) and adds a file, canonically.
    let b = alpha_commits(
        repo,
        &[("beta.txt", "alpha-rewrote-beta\n"), ("gamma.txt", "new\n")],
        "B",
    );
    assert!(
        !git(repo, &["ls-files"]).contains(".doubledoor"),
        "`git add -A` in the canonical worktree must never capture Doubledoor state"
    );

    // The watcher observes once.
    let out = dd(repo, &["watch", "--once", "--format", "json"]);
    let obs: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(obs["session"], id);
    assert_eq!(obs["new_ghosts"].as_array().unwrap().len(), 1);
    assert_eq!(obs["cursor"], b);
    assert_eq!(obs["state"], "GhostPending");

    // Structured notification on the event stream (§13 shape).
    let evs = events(repo, &[]);
    let avail = evs
        .iter()
        .find(|e| e["event"] == "ghost.available")
        .expect("ghost.available event");
    assert_eq!(avail["session"], id);
    assert_eq!(avail["source_commit"], b);
    assert_eq!(avail["source_actor"], "agent-alpha");
    assert_eq!(avail["classification"], "read_relevant");
    assert_eq!(avail["intersections"], serde_json::json!(["file:beta.txt"]));
    assert_eq!(avail["needs_assessment"], true);
    assert!(evs
        .iter()
        .any(|e| e["event"] == "session.state" && e["to"] == "GhostPending"));

    // The workspace was not altered in any way.
    assert_eq!(
        snapshot(&ws),
        before,
        "observation must never touch the workspace"
    );
    assert!(!ws.join("gamma.txt").exists());

    // `ghosts` explains it (§31/§32) and a second pass records nothing new.
    let listed = ghosts(repo, &id);
    assert_eq!(listed.len(), 1);
    let g = &listed[0];
    assert_eq!(g.status, "pending");
    assert_eq!(g.intent.as_deref(), Some("add retry semantics"));
    assert_eq!(g.affected, ["file:beta.txt", "file:gamma.txt"]);
    assert_eq!(g.safe_imports, ["file:beta.txt", "file:gamma.txt"]);
    assert!(g.held.is_empty());
    let human = dd(repo, &["ghosts", &id]);
    for needle in [
        "GHOST gh_",
        "SOURCE",
        "agent-alpha",
        "INTERSECTION",
        "ACTION REQUIRED",
    ] {
        assert!(human.contains(needle), "missing {needle}:\n{human}");
    }
    assert_eq!(snapshot(&ws), before);

    // DD-I3: the base did not move; canonical HEAD is α's commit.
    let inspect: serde_json::Value =
        serde_json::from_str(&dd(repo, &["inspect", &id, "--format", "json"])).unwrap();
    assert_eq!(inspect["base_commit"], beta.base_commit.to_string());
    assert_eq!(fx.head(), b);

    let kinds: Vec<String> = journal_lines(repo, &id).iter().map(journal_kind).collect();
    for k in ["GhostRecorded", "GhostCursorAdvanced", "GhostObserved"] {
        assert!(
            kinds.contains(&k.to_string()),
            "journal lacks {k}: {kinds:?}"
        );
    }
}

#[test]
fn defer_then_accept_imports_only_on_request() {
    let fx = Fixture::new();
    let repo = fx.root();
    let beta = open_session_with(
        repo,
        "agent-beta",
        "alpha.txt",
        "agent-alpha",
        &["--read", "beta.txt"],
    );
    let id = beta.id.to_string();
    let ws = workspace(&beta);
    let before = snapshot(&ws);
    alpha_commits(repo, &[("beta.txt", "alpha-rewrote-beta\n")], "B");
    let gid = ghosts(repo, &id)[0].id.to_string();

    // DEFER: acknowledged, session continues, workspace untouched.
    let d = decide(repo, "defer", &gid, &[]);
    assert_eq!(d.decision, "deferred");
    assert_eq!(d.state, DoorState::Active);
    assert_eq!(d.outstanding, 0);
    assert_eq!(snapshot(&ws), before);
    assert_eq!(ghosts(repo, &id)[0].status, "deferred");
    assert_eq!(doubledoor(repo, &["defer", &gid]).status.code(), Some(6));

    // ACCEPT (later, from ACTIVE): the change enters as a new workspace commit.
    let a = decide(repo, "accept", &gid, &[]);
    assert_eq!(a.decision, "accepted");
    assert_eq!(a.state, DoorState::Active);
    let commit = a.applied_commit.expect("applied commit").to_string();
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), commit);
    assert_eq!(git(&ws, &["rev-parse", "HEAD~1"]), before.0);
    assert_eq!(read(&ws.join("beta.txt")), "alpha-rewrote-beta\n");
    assert_eq!(
        ghosts(repo, &id)[0]
            .applied_commit
            .as_ref()
            .unwrap()
            .to_string(),
        commit
    );

    for verb in ["accept", "defer", "reject"] {
        assert_eq!(
            doubledoor(repo, &[verb, &gid]).status.code(),
            Some(6),
            "{verb} after accept"
        );
    }
    let evs = events(repo, &["--session", &id]);
    let decisions: Vec<&str> = evs
        .iter()
        .filter(|e| e["event"] == "ghost.assessed")
        .map(|e| e["decision"].as_str().unwrap())
        .collect();
    assert_eq!(decisions, ["deferred", "accepted"]);
}

#[test]
fn selective_accept_keeps_the_held_surface() {
    let fx = Fixture::new();
    let repo = fx.root();
    let beta = open_session_with(repo, "agent-beta", "alpha.txt", "agent-alpha", &[]);
    let id = beta.id.to_string();
    let ws = workspace(&beta);
    alpha_commits(
        repo,
        &[
            ("alpha.txt", "alpha-version\n"),
            ("beta.txt", "alpha-rewrote-beta\n"),
        ],
        "B",
    );
    let g = &ghosts(repo, &id)[0];
    assert_eq!(g.classification, "write_overlap");
    assert_eq!(g.held, ["file:alpha.txt"]);
    assert_eq!(g.safe_imports, ["file:beta.txt"]);
    assert_eq!(state_of(repo, &id), DoorState::GhostPending);

    // A surface the ghost did not touch is a usage error; nothing changes.
    let out = doubledoor(
        repo,
        &["accept", &g.id.to_string(), "--surface", "nope.txt"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(state_of(repo, &id), DoorState::GhostPending);

    let d = decide(
        repo,
        "accept",
        &g.id.to_string(),
        &["--surface", "beta.txt"],
    );
    assert_eq!(d.surfaces, ["file:beta.txt"]);
    assert_eq!(d.state, DoorState::Active);
    assert_eq!(read(&ws.join("beta.txt")), "alpha-rewrote-beta\n");
    assert_eq!(
        read(&ws.join("alpha.txt")),
        "base-alpha\n",
        "held surface stays β's"
    );
}

#[test]
fn conflicting_accept_changes_nothing_then_reject_resumes() {
    let fx = Fixture::new();
    let repo = fx.root();
    let beta = open_session_with(repo, "agent-beta", "alpha.txt", "agent-alpha", &[]);
    let id = beta.id.to_string();
    let ws = workspace(&beta);
    fs::write(ws.join("alpha.txt"), "beta-in-progress\n").unwrap();
    let before = snapshot(&ws);
    let canonical = alpha_commits(repo, &[("alpha.txt", "alpha-version\n")], "B");
    let gid = ghosts(repo, &id)[0].id.to_string();

    let out = doubledoor(repo, &["accept", &gid]);
    assert_eq!(
        out.status.code(),
        Some(10),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        snapshot(&ws),
        before,
        "a failed accept leaves the workspace as it was"
    );
    assert_eq!(state_of(repo, &id), DoorState::Assessing);
    assert_eq!(ghosts(repo, &id)[0].status, "pending");

    let d = decide(repo, "reject", &gid, &[]);
    assert_eq!(d.state, DoorState::Active);
    assert_eq!(snapshot(&ws), before);
    assert_eq!(
        fx.head(),
        canonical,
        "rejection never reverts canonical history"
    );
}

#[test]
fn independent_commits_notify_without_blocking() {
    let fx = Fixture::new();
    let repo = fx.root();
    let beta = open_session_with(repo, "agent-beta", "alpha.txt", "agent-alpha", &[]);
    let id = beta.id.to_string();
    alpha_commits(repo, &[("unrelated.txt", "x\n")], "B");
    dd(repo, &["watch", "--once"]);

    assert_eq!(state_of(repo, &id), DoorState::Active);
    let g = &ghosts(repo, &id)[0];
    assert_eq!(g.classification, "independent");
    assert!(!g.needs_assessment);
    let evs = events(repo, &[]);
    assert_eq!(
        evs.len(),
        1,
        "only ghost.available, no state change: {evs:?}"
    );
    assert_eq!(evs[0]["needs_assessment"], false);
}

#[test]
fn watch_covers_every_live_session_and_events_resume_by_sequence() {
    let fx = Fixture::new();
    let repo = fx.root();
    let a = open_session_with(repo, "agent-beta", "alpha.txt", "agent-alpha", &[]);
    let b = open_session_with(repo, "agent-gamma", "beta.txt", "agent-alpha", &[]);
    let gone = open_session_with(repo, "agent-delta", "beta.txt", "agent-alpha", &[]);
    dd(repo, &["abandon", &gone.id.to_string()]);

    alpha_commits(repo, &[("alpha.txt", "v2\n")], "B");
    let out = dd(repo, &["watch", "--once", "--format", "json"]);
    assert_eq!(
        out.lines().count(),
        2,
        "abandoned sessions are not observed:\n{out}"
    );
    assert_eq!(state_of(repo, &a.id.to_string()), DoorState::GhostPending);
    assert_eq!(state_of(repo, &b.id.to_string()), DoorState::Active);

    let all = events(repo, &[]);
    let last = all.last().unwrap()["seq"].as_u64().unwrap();
    assert_eq!(all.len() as u64, last, "sequence numbers are dense");
    assert!(events(repo, &["--since", &last.to_string()]).is_empty());
    assert_eq!(events(repo, &["--max", "1"]).len(), 1);
    let only_b = events(repo, &["--session", &b.id.to_string()]);
    assert!(only_b.iter().all(|e| e["session"] == b.id.to_string()));

    // Nothing moved: a second pass observes nothing and emits nothing.
    assert!(dd(repo, &["watch", "--once", "--format", "json"]).is_empty());
    assert_eq!(events(repo, &[]).len(), all.len());

    // Abandoned sessions cannot assess ghosts.
    let gid = ghosts(repo, &a.id.to_string())[0].id.to_string();
    dd(repo, &["abandon", &a.id.to_string()]);
    assert_eq!(doubledoor(repo, &["accept", &gid]).status.code(), Some(6));
}

#[test]
fn recover_adopts_an_accept_that_was_committed_but_not_journalled() {
    let fx = Fixture::new();
    let repo = fx.root();
    let beta = open_session_with(repo, "agent-beta", "alpha.txt", "agent-alpha", &[]);
    let id = beta.id.to_string();
    let ws = workspace(&beta);
    alpha_commits(repo, &[("beta.txt", "v2\n")], "B");
    let gid = ghosts(repo, &id)[0].id.to_string();

    // Simulate a crash after the workspace commit, before GhostAssessed.
    fs::write(ws.join("beta.txt"), "v2\n").unwrap();
    git(
        &ws,
        &[
            "commit",
            "-qam",
            "doubledoor: accept ghost",
            "--trailer",
            &format!("Dd-Ghost: {gid}"),
        ],
    );
    assert_eq!(ghosts(repo, &id)[0].status, "pending");

    let report: serde_json::Value =
        serde_json::from_str(&dd(repo, &["recover", "--format", "json"])).unwrap();
    let actions = report["actions"].as_array().unwrap();
    assert!(
        actions
            .iter()
            .any(|a| a["action"] == "adopted-ghost-accept"),
        "{actions:?}"
    );
    let g = &ghosts(repo, &id)[0];
    assert_eq!(g.status, "accepted");
    assert_eq!(
        g.applied_commit.as_ref().unwrap().to_string(),
        git(&ws, &["rev-parse", "HEAD"])
    );
    // Idempotent: a second recovery adopts nothing.
    let again: serde_json::Value =
        serde_json::from_str(&dd(repo, &["recover", "--format", "json"])).unwrap();
    assert!(again["actions"].as_array().unwrap().is_empty());
}
