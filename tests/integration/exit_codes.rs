//! Stable exit-code coverage (§30): every documented code is reachable and
//! nothing else leaks through.

mod common;

use common::{dd, doubledoor, open_session, sessions, write_notice, Fixture};

#[test]
fn not_found_session_exits_5() {
    let fx = Fixture::new();
    let repo = fx.root();
    let out = doubledoor(
        repo,
        &["inspect", "dd_00000000-0000-0000-0000-000000000000"],
    );
    assert_eq!(out.status.code(), Some(5));
    let out = doubledoor(
        repo,
        &["abandon", "dd_00000000-0000-0000-0000-000000000000"],
    );
    assert_eq!(out.status.code(), Some(5));
}

#[test]
fn phase_ii_subcommands_require_their_target() {
    let fx = Fixture::new();
    let repo = fx.root();
    // Missing positional argument: a usage error, not "not implemented".
    for sub in ["ghosts", "accept", "defer", "reject"] {
        let out = doubledoor(repo, &[sub]);
        assert_eq!(out.status.code(), Some(1), "{sub} without a target");
    }
    // Well-formed but unknown ids: not found.
    let out = doubledoor(repo, &["ghosts", "dd_00000000-0000-0000-0000-000000000000"]);
    assert_eq!(out.status.code(), Some(5));
    for sub in ["accept", "defer", "reject"] {
        let out = doubledoor(repo, &[sub, "gh_00000000-0000-0000-0000-000000000000"]);
        assert_eq!(out.status.code(), Some(5), "{sub} unknown ghost");
        let out = doubledoor(repo, &[sub, "not-a-ghost"]);
        assert_eq!(out.status.code(), Some(5), "{sub} malformed ghost id");
    }
}

#[test]
fn phase_iv_subcommands_require_their_target() {
    let fx = Fixture::new();
    let repo = fx.root();
    let unknown = "dd_00000000-0000-0000-0000-000000000000";
    for sub in ["reconcile", "validate", "candidate", "discard", "promote"] {
        let out = doubledoor(repo, &[sub]);
        assert_eq!(out.status.code(), Some(1), "{sub} without a session");
    }
    for args in [
        vec!["reconcile", unknown],
        vec!["validate", unknown],
        vec!["candidate", unknown],
        vec!["discard", unknown],
        vec!["promote", unknown, "--lease", "lease.json"],
    ] {
        let out = doubledoor(repo, &args);
        assert_eq!(out.status.code(), Some(5), "{args:?} on an unknown session");
    }
}

#[test]
fn open_without_notice_exits_3() {
    let fx = Fixture::new();
    let repo = fx.root();
    let out = doubledoor(
        repo,
        &[
            "open",
            "--actor",
            "agent-beta",
            "--intent",
            "no notice",
            "--surface",
            "alpha.txt",
        ],
    );
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn open_with_garbage_notice_exits_3() {
    let fx = Fixture::new();
    let repo = fx.root();
    let notice = repo.join("garbage.json");
    std::fs::write(&notice, "{not valid json").unwrap();
    let out = doubledoor(
        repo,
        &[
            "open",
            "--actor",
            "agent-beta",
            "--intent",
            "bad notice",
            "--surface",
            "alpha.txt",
            "--notice",
            notice.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn holder_cannot_be_requester_exits_3() {
    let fx = Fixture::new();
    let repo = fx.root();
    let notice = write_notice(repo, "self", "agent-alpha", "alpha.txt", &fx.head());
    let out = doubledoor(
        repo,
        &[
            "open",
            "--actor",
            "agent-alpha",
            "--intent",
            "self-contention",
            "--surface",
            "alpha.txt",
            "--notice",
            notice.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn policy_deny_exits_2() {
    let fx = Fixture::new();
    let repo = fx.root();
    let policy = r#"{"schema":{"major":1,"minor":0},"payload":{"max_active_sessions":1,"max_workspace_bytes":1099511627776,"max_session_lifetime_secs":604800}}"#;
    std::fs::create_dir_all(repo.join(".doubledoor")).unwrap();
    std::fs::write(repo.join(".doubledoor/policy.json"), policy).unwrap();

    let first = open_session(repo, "agent-alpha", "a", "alpha.txt", "agent-beta");
    assert_eq!(first.state, doubledoor_core::DoorState::Active);

    let notice = write_notice(repo, "beta", "agent-alpha", "beta.txt", &fx.head());
    let out = doubledoor(
        repo,
        &[
            "open",
            "--actor",
            "agent-beta",
            "--intent",
            "over quota",
            "--surface",
            "beta.txt",
            "--notice",
            notice.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(2), "policy deny must exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("policy denied"), "stderr: {stderr}");
}

#[test]
fn corrupt_metadata_exits_7() {
    let fx = Fixture::new();
    let repo = fx.root();
    let session = open_session(repo, "agent-alpha", "a", "alpha.txt", "agent-beta");
    let id = session.id.as_str();

    // Truncate metadata.cbor to garbage.
    std::fs::write(fx.session_dir(&id).join("metadata.cbor"), b"\x99garbage").unwrap();

    let out = doubledoor(repo, &["inspect", &id]);
    assert_eq!(out.status.code(), Some(7), "corrupt metadata must exit 7");

    let out = doubledoor(repo, &["sessions"]);
    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn usage_error_exits_1() {
    let fx = Fixture::new();
    let repo = fx.root();
    let out = doubledoor(repo, &["open", "--bogus-flag"]);
    assert_eq!(out.status.code(), Some(1));

    // No git repository above the fixture's tempdir parent chain is
    // guaranteed, so assert the not-a-repository mapping via --repo instead.
    let out = doubledoor(repo, &["--repo", "/nonexistent/path", "status"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn help_lists_the_exit_code_contract() {
    let fx = Fixture::new();
    let repo = fx.root();
    let out = dd(repo, &["--help"]);
    for code in [
        "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12",
    ] {
        assert!(out.contains(code), "--help must document exit code {code}");
    }
}

#[test]
fn sessions_lists_opened_sessions() {
    let fx = Fixture::new();
    let repo = fx.root();
    open_session(repo, "agent-alpha", "a", "alpha.txt", "agent-beta");
    open_session(repo, "agent-beta", "b", "beta.txt", "agent-alpha");
    let summaries = sessions(repo);
    assert_eq!(summaries.len(), 2);
    let human = dd(repo, &["sessions"]);
    assert!(human.contains("agent-alpha"));
    assert!(human.contains("agent-beta"));
}
