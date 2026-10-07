//! §27 hook suppression at the CLI level: internal git operations must never
//! fire user hooks.

mod common;

use common::{git, open_session, Fixture};

#[test]
fn open_never_fires_user_hooks() {
    let fx = Fixture::new();
    let repo = fx.root();

    // A post-checkout bomb: records every firing in a sentinel file.
    let sentinel = repo.join("hook-sentinel.txt");
    let hooks_dir = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\necho fired >> {}\n", sentinel.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Doubledoor's `open` performs a `git worktree add` (which normally
    // fires post-checkout) — the sentinel must stay untouched.
    let session = open_session(
        repo,
        "agent-alpha",
        "edit alpha.txt",
        "alpha.txt",
        "agent-beta",
    );
    assert!(
        !sentinel.exists(),
        "post-checkout hook fired during `doubledoor open` (§27)"
    );
    assert_eq!(session.state, doubledoor_core::DoorState::Active);

    // Control: a raw `git worktree add` in the same repo does fire the hook,
    // proving suppression (not a broken hook) is what kept it silent.
    let raw = repo.join("raw-control-worktree");
    git(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            raw.to_str().unwrap(),
            &fx.head(),
        ],
    );
    assert!(
        sentinel.exists(),
        "control run should have fired the hook — is the fixture hook valid?"
    );
    git(
        repo,
        &["worktree", "remove", "--force", raw.to_str().unwrap()],
    );
}
