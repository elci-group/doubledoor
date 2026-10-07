//! Shared fixtures for the Doubledoor CLI integration tests.
//!
//! Every test drives the real `doubledoor` binary against a real Git
//! repository inside a `tempfile` temporary directory. No mocking: the exit
//! criterion (§45) is only meaningful against actual Git behavior.
//!
//! Each integration test crate compiles this module independently, so not
//! every helper is used by every crate.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use doubledoor_protocol::SessionSummary;

/// A freshly initialized Git repository with two tracked files committed at
/// base commit **A**. Deleted on drop.
pub struct Fixture {
    pub dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "."]);
        git(root, &["config", "user.email", "test@doubledoor.dev"]);
        git(root, &["config", "user.name", "Doubledoor Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("alpha.txt"), "base-alpha\n").unwrap();
        fs::write(root.join("beta.txt"), "base-beta\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "A"]);
        Fixture { dir }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn head(&self) -> String {
        git(self.root(), &["rev-parse", "HEAD"])
    }

    /// `.doubledoor/sessions/<id>`.
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.root().join(".doubledoor/sessions").join(id)
    }
}

/// Raw git invocation (no Doubledoor machinery) for fixture setup and
/// assertions. Panics on failure.
pub fn git(repo: &Path, args: &[impl AsRef<std::ffi::OsStr> + std::fmt::Debug]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed (code {:?}): {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Path to the `doubledoor` binary under test. Cargo builds it before these
/// suites run because they are declared as tests of `doubledoor-cli`.
pub fn doubledoor_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_doubledoor"))
}

/// Run the CLI against `repo` (as cwd, exercising default repo discovery
/// unless `--repo` is passed explicitly). Returns the full process output.
pub fn doubledoor(repo: &Path, args: &[&str]) -> Output {
    Command::new(doubledoor_bin())
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn doubledoor")
}

/// Run the CLI, assert success (exit 0), and return stdout as a String.
pub fn dd(repo: &Path, args: &[&str]) -> String {
    let out = doubledoor(repo, args);
    assert!(
        out.status.success(),
        "doubledoor {args:?} failed (code {:?}): {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Write a Locksmith contention notice JSON file into the fixture repo and
/// return its path. `holder` contends with `surface` at `base_commit`.
pub fn write_notice(
    repo: &Path,
    name: &str,
    holder: &str,
    surface: &str,
    base_commit: &str,
) -> PathBuf {
    let path = repo.join(format!("notice-{name}.json"));
    let json = format!(
        r#"{{"holder":"{holder}","requested":["{surface}"],"contested":["{surface}"],"lease_id":"550e8400-e29b-41d4-a716-446655440000","base_commit":"{base_commit}"}}"#
    );
    fs::write(&path, json).unwrap();
    path
}

/// Open a session via the real CLI and parse the JSON SessionSummary.
pub fn open_session(
    repo: &Path,
    actor: &str,
    intent: &str,
    surface: &str,
    notice_holder: &str,
) -> SessionSummary {
    let base = git(repo, &["rev-parse", "HEAD"]);
    let notice = write_notice(repo, actor, notice_holder, surface, &base);
    let out = dd(
        repo,
        &[
            "open",
            "--actor",
            actor,
            "--intent",
            intent,
            "--surface",
            surface,
            "--notice",
            notice.to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    serde_json::from_str(&out).expect("SessionSummary JSON")
}

/// Open a session with extra `open` flags (e.g. `--read beta.txt`).
pub fn open_session_with(
    repo: &Path,
    actor: &str,
    surface: &str,
    notice_holder: &str,
    extra: &[&str],
) -> SessionSummary {
    let base = git(repo, &["rev-parse", "HEAD"]);
    let notice = write_notice(repo, actor, notice_holder, surface, &base);
    let mut args = vec![
        "open",
        "--actor",
        actor,
        "--intent",
        "speculative work",
        "--surface",
        surface,
        "--notice",
        notice.to_str().unwrap(),
        "--format",
        "json",
    ];
    args.extend_from_slice(extra);
    serde_json::from_str(&dd(repo, &args)).expect("SessionSummary JSON")
}

/// All sessions via `sessions --format json`.
pub fn sessions(repo: &Path) -> Vec<SessionSummary> {
    let out = dd(repo, &["sessions", "--format", "json"]);
    serde_json::from_str(&out).expect("Vec<SessionSummary> JSON")
}

/// Extract the journal event kind name from one JSONL envelope line. Kinds
/// serialize either as a bare string (`"SessionCreated"`) or as a
/// single-key object (`{"WorkspaceCreated": {...}}`).
pub fn journal_kind(line: &serde_json::Value) -> String {
    let event = &line["payload"]["event"];
    if let Some(s) = event.as_str() {
        s.to_string()
    } else {
        event
            .as_object()
            .and_then(|o| o.keys().next().cloned())
            .expect("journal event kind")
    }
}

pub fn journal_lines(repo: &Path, id: &str) -> Vec<serde_json::Value> {
    let path = repo
        .join(".doubledoor/sessions")
        .join(id)
        .join("journal/events.jsonl");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("journal line JSON"))
        .collect()
}

pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
