//! The §45 exit-criterion scenario: two actors safely mutate independent
//! speculative environments from the same canonical state, with no
//! cross-contamination.

mod common;

use doubledoor_core::DoorState;

use common::{
    dd, doubledoor, git, journal_kind, journal_lines, open_session, read, sessions, Fixture,
};

/// Plan test 1 — full two-actor isolation against a repo seeded at commit A.
#[test]
fn two_actors_isolate_from_same_base() {
    let fx = Fixture::new();
    let repo = fx.root();
    let head_a = fx.head();

    // Both actors were redirected into Doubledoor by a Locksmith contention
    // notice; both sessions open from the same base commit A.
    let alpha = open_session(
        repo,
        "agent-alpha",
        "edit alpha.txt",
        "alpha.txt",
        "agent-beta",
    );
    let beta = open_session(
        repo,
        "agent-beta",
        "edit beta.txt",
        "beta.txt",
        "agent-alpha",
    );

    assert_eq!(alpha.state, DoorState::Active);
    assert_eq!(beta.state, DoorState::Active);
    assert_eq!(alpha.base_commit.as_str(), head_a);
    assert_eq!(beta.base_commit.as_str(), head_a);

    let ws_alpha = alpha.workspace.clone().expect("alpha workspace");
    let ws_beta = beta.workspace.clone().expect("beta workspace");

    // Mutate disjoint files in each workspace.
    std::fs::write(ws_alpha.join("alpha.txt"), "alpha edit\n").unwrap();
    std::fs::write(ws_beta.join("beta.txt"), "beta edit\n").unwrap();

    // DD-I1: canonical HEAD unmoved by speculative mutation.
    assert_eq!(fx.head(), head_a, "canonical HEAD must not move");

    // DD-I2: each workspace sees only its own edits.
    assert_eq!(read(&ws_alpha.join("alpha.txt")), "alpha edit\n");
    assert_eq!(read(&ws_alpha.join("beta.txt")), "base-beta\n");
    assert_eq!(read(&ws_beta.join("beta.txt")), "beta edit\n");
    assert_eq!(read(&ws_beta.join("alpha.txt")), "base-alpha\n");

    // §9: the user's active worktree carries no tracked modifications —
    // only untracked additions (.doubledoor bookkeeping, notice files).
    let status = git(repo, &["status", "--porcelain"]);
    assert!(
        status.lines().all(|l| l.starts_with("??")),
        "canonical worktree polluted: {status}"
    );
}

/// §45 contamination guard: α commits inside its speculative workspace; β's
/// workspace content stays byte-identical to the base tree. External mutation
/// never enters without a recorded transition.
#[test]
fn alpha_commit_never_enters_beta_workspace() {
    let fx = Fixture::new();
    let repo = fx.root();
    let head_a = fx.head();

    let alpha = open_session(
        repo,
        "agent-alpha",
        "edit alpha.txt",
        "alpha.txt",
        "agent-beta",
    );
    let beta = open_session(
        repo,
        "agent-beta",
        "edit beta.txt",
        "beta.txt",
        "agent-alpha",
    );

    let ws_alpha = alpha.workspace.clone().expect("alpha workspace");
    let ws_beta = beta.workspace.clone().expect("beta workspace");

    // α edits and commits inside its own worktree.
    std::fs::write(ws_alpha.join("alpha.txt"), "alpha speculative commit\n").unwrap();
    git(&ws_alpha, &["add", "alpha.txt"]);
    git(&ws_alpha, &["commit", "-qm", "alpha speculative"]);

    // β's workspace is untouched, byte-identical to base.
    assert_eq!(read(&ws_beta.join("alpha.txt")), "base-alpha\n");
    assert_eq!(read(&ws_beta.join("beta.txt")), "base-beta\n");
    assert_eq!(
        git(&ws_beta, &["status", "--porcelain"]),
        "",
        "beta workspace must be clean after alpha's commit"
    );

    // Canonical HEAD still at A: α's speculative commit is not on any branch.
    assert_eq!(fx.head(), head_a);

    // DD-I3: β's recorded base is still commit A after α's commit.
    let beta_summary = sessions(repo)
        .into_iter()
        .find(|s| s.id == beta.id)
        .expect("beta session");
    assert_eq!(beta_summary.base_commit.as_str(), head_a);
}

/// Journalling half of the exit criterion: β's journal records
/// SessionCreated → WorkspaceCreated → SessionActivated in order, and
/// `inspect` reports ACTIVE.
#[test]
fn session_is_journaled_in_order() {
    let fx = Fixture::new();
    let repo = fx.root();

    let beta = open_session(
        repo,
        "agent-beta",
        "edit beta.txt",
        "beta.txt",
        "agent-alpha",
    );

    let lines = journal_lines(repo, &beta.id.as_str());
    let kinds: Vec<String> = lines.iter().map(journal_kind).collect();
    assert_eq!(
        kinds,
        vec!["SessionCreated", "WorkspaceCreated", "SessionActivated"],
        "journal order: {kinds:?}"
    );

    let inspect = dd(repo, &["inspect", &beta.id.as_str()]);
    assert!(
        inspect.contains("STATE=ACTIVE"),
        "inspect must report ACTIVE:\n{inspect}"
    );
    assert!(inspect.contains("JOURNAL_EVENTS=3"));

    // JSON inspect carries the same facts.
    let json: serde_json::Value = serde_json::from_str(&dd(
        repo,
        &["inspect", &beta.id.as_str(), "--format", "json"],
    ))
    .unwrap();
    assert_eq!(json["state"], "Active");
    assert_eq!(json["journal_events"], 3);
    assert_eq!(json["workspace_exists"], true);
}

/// `status` aggregates the session population.
#[test]
fn status_reports_active_and_total() {
    let fx = Fixture::new();
    let repo = fx.root();
    open_session(repo, "agent-alpha", "a", "alpha.txt", "agent-beta");
    open_session(repo, "agent-beta", "b", "beta.txt", "agent-alpha");

    let json: serde_json::Value =
        serde_json::from_str(&dd(repo, &["status", "--format", "json"])).unwrap();
    assert_eq!(json["active"], 2);
    assert_eq!(json["total"], 2);
    assert_eq!(
        json["schema"],
        doubledoor_protocol::CURRENT_SCHEMA.to_string()
    );
    assert_eq!(json["sessions"].as_array().unwrap().len(), 2);

    let human = dd(repo, &["status"]);
    assert!(human.contains("ACTIVE=2"), "human status:\n{human}");
    assert!(human.contains("TOTAL=2"));
}

/// Default repo discovery: run from a nested subdirectory with no `--repo`.
#[test]
fn repo_discovery_walks_upward_from_cwd() {
    let fx = Fixture::new();
    let repo = fx.root();
    let nested = repo.join("src/deep");
    std::fs::create_dir_all(&nested).unwrap();

    let out = doubledoor(&nested, &["sessions", "--format", "json"]);
    assert!(
        out.status.success(),
        "discovery from nested dir failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
