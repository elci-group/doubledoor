//! Abandon semantics: worktree removal, ABANDONED state, journalled evidence.

mod common;

use doubledoor_core::DoorState;

use common::{dd, doubledoor, git, journal_lines, open_session, sessions, Fixture};

#[test]
fn abandon_removes_workspace_and_records_it() {
    let fx = Fixture::new();
    let repo = fx.root();
    let session = open_session(
        repo,
        "agent-alpha",
        "edit alpha.txt",
        "alpha.txt",
        "agent-beta",
    );
    let id = session.id.as_str();
    let ws = session.workspace.clone().expect("workspace");
    assert!(ws.is_dir());

    // Human output is the §31 KEY=VALUE shape.
    let human = dd(repo, &["abandon", &id]);
    assert!(human.contains(&format!("SESSION={id}")), "{human}");
    assert!(human.contains("STATE=ABANDONED"), "{human}");

    // The worktree is deregistered and the directory is gone.
    assert!(!ws.exists(), "workspace directory must be removed");
    let list = git(repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.lines().any(|l| l.contains(ws.to_str().unwrap())),
        "worktree still registered:\n{list}"
    );

    // The session remains visible as ABANDONED with no workspace.
    let summaries = sessions(repo);
    let summary = summaries
        .iter()
        .find(|s| s.id == session.id)
        .expect("session still listed");
    assert_eq!(summary.state, DoorState::Abandoned);
    assert_eq!(summary.workspace, None);

    // Metadata and journal survive the abandon.
    assert!(fx.session_dir(&id).join("metadata.cbor").is_file());
    let kinds: Vec<String> = journal_lines(repo, &id)
        .iter()
        .map(|l| {
            let e = &l["payload"]["event"];
            e.as_str()
                .or_else(|| e.as_object()?.keys().next().map(String::as_str))
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(
        kinds.contains(&"SessionAbandoned".to_string()),
        "journal must record SessionAbandoned: {kinds:?}"
    );
}

#[test]
fn abandon_twice_is_a_typed_error() {
    let fx = Fixture::new();
    let repo = fx.root();
    let session = open_session(
        repo,
        "agent-alpha",
        "edit alpha.txt",
        "alpha.txt",
        "agent-beta",
    );
    let id = session.id.as_str();
    dd(repo, &["abandon", &id]);

    let out = doubledoor(repo, &["abandon", &id]);
    assert_eq!(out.status.code(), Some(6), "second abandon must exit 6");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid state transition"),
        "stderr: {stderr}"
    );
}

#[test]
fn abandoned_session_frees_policy_quota() {
    let fx = Fixture::new();
    let repo = fx.root();
    let policy = r#"{"schema":{"major":1,"minor":0},"payload":{"max_active_sessions":1,"max_workspace_bytes":1099511627776,"max_session_lifetime_secs":604800}}"#;
    std::fs::create_dir_all(repo.join(".doubledoor")).unwrap();
    std::fs::write(repo.join(".doubledoor/policy.json"), policy).unwrap();

    let first = open_session(repo, "agent-alpha", "a", "alpha.txt", "agent-beta");
    // Quota saturated: a second open is denied.
    let denied = doubledoor(
        repo,
        &[
            "open",
            "--actor",
            "agent-beta",
            "--intent",
            "b",
            "--surface",
            "beta.txt",
            "--notice",
            common::write_notice(repo, "beta", "agent-alpha", "beta.txt", &fx.head())
                .to_str()
                .unwrap(),
        ],
    );
    assert_eq!(denied.status.code(), Some(2), "quota deny must exit 2");

    // After abandoning, the quota frees up.
    dd(repo, &["abandon", &first.id.as_str()]);
    let second = open_session(repo, "agent-beta", "b", "beta.txt", "agent-alpha");
    assert_eq!(second.state, DoorState::Active);
}
