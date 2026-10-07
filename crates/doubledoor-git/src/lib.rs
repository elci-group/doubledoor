//! Git CLI integration: repository discovery, canonical state capture, and
//! per-session workspace isolation via `git worktree` (§9).
//!
//! All Git access goes through [`Git::run_git`], a single choke point that
//! executes the `git` binary with structured arguments only (never a shell),
//! hooks disabled via `-c core.hooksPath=/dev/null`, and advisory locks off
//! via `GIT_OPTIONAL_LOCKS=0` — so internal operations never fire user hooks
//! or contend with repository locks (§27).

#![forbid(unsafe_code)]

mod error;

pub use error::GitError;

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use doubledoor_core::{CanonicalState, DoorSessionId, GitOid, WorkspaceRef};
use tracing::warn;

/// On-disk layout of a session's speculative workspace, relative to the
/// repository root (§9).
const SESSIONS_DIR: &str = ".doubledoor/sessions";
const WORKSPACE_DIR_NAME: &str = "workspace";

/// Gateway to a Git repository. Construct it at a repository root with
/// [`Git::new`] or locate the enclosing repository with [`Git::discover`].
#[derive(Debug, Clone)]
pub struct Git {
    repo_root: PathBuf,
}

impl Git {
    /// Bind to a repository root. No I/O is performed until the first call.
    pub fn new(repo_root: PathBuf) -> Self {
        Git { repo_root }
    }

    /// The repository root this instance operates on.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Locate the enclosing Git repository by running
    /// `git rev-parse --show-toplevel` from `start`. This is plain repo
    /// discovery only — it does not require or create any `.doubledoor`
    /// bookkeeping.
    pub fn discover(start: &Path) -> Result<Self, GitError> {
        let out = Self::base_command()
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(start)
            .output()
            .map_err(|_| GitError::NotARepository(start.to_path_buf()))?;
        if !out.status.success() {
            return Err(GitError::NotARepository(start.to_path_buf()));
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if root.is_empty() {
            return Err(GitError::UnexpectedOutput(
                "rev-parse --show-toplevel returned empty output".to_string(),
            ));
        }
        Ok(Git::new(PathBuf::from(root)))
    }

    /// The `git` invocation template shared by every call site: hooks are
    /// routed to `/dev/null` so they can never fire during internal ops (§27),
    /// and optional lock-taking is disabled so read-only commands never block
    /// on or corrupt repository locks.
    fn base_command() -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-c").arg("core.hooksPath=/dev/null");
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        cmd
    }

    /// Run `git` with the given structured arguments inside the repository
    /// root. This is the single choke point for all Git I/O: no shell is ever
    /// involved, hooks never fire, and failures surface as structured
    /// [`GitError::Command`] values carrying argv, exit code, and stderr.
    /// Returns trimmed stdout on success.
    pub fn run_git(&self, args: &[&str]) -> Result<String, GitError> {
        let rendered: Vec<String> = args.iter().map(ToString::to_string).collect();
        let out = Self::base_command()
            .args(args)
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| GitError::Command {
                args: rendered.clone(),
                code: None,
                stderr: e.to_string(),
            })?;
        if !out.status.success() {
            return Err(GitError::Command {
                args: rendered,
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// [`Git::run_git`] returning stdout byte-for-byte (untrimmed, not UTF-8
    /// decoded) — for payloads such as patches where the trailing newline
    /// and binary hunks are significant. Same choke-point guarantees.
    pub fn run_git_bytes(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let rendered: Vec<String> = args.iter().map(ToString::to_string).collect();
        let out = Self::base_command()
            .args(args)
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| GitError::Command {
                args: rendered.clone(),
                code: None,
                stderr: e.to_string(),
            })?;
        if !out.status.success() {
            return Err(GitError::Command {
                args: rendered,
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(out.stdout)
    }

    /// [`Git::run_git`] with extra environment variables — used to build
    /// trees in a private index (`GIT_INDEX_FILE`) so candidate construction
    /// never touches a real index, worktree, or ref. Same choke-point
    /// guarantees; the variables are passed structurally, never via a shell.
    pub fn run_git_env(&self, env: &[(&str, &str)], args: &[&str]) -> Result<String, GitError> {
        let rendered: Vec<String> = args.iter().map(ToString::to_string).collect();
        let mut cmd = Self::base_command();
        cmd.args(args).current_dir(&self.repo_root);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().map_err(|e| GitError::Command {
            args: rendered.clone(),
            code: None,
            stderr: e.to_string(),
        })?;
        if !out.status.success() {
            return Err(GitError::Command {
                args: rendered,
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Run `git` and return `(exit code, stdout bytes, stderr)` without
    /// treating a non-zero exit as an error — for commands whose exit code
    /// carries data (`git merge-file` returns the number of conflicts).
    pub fn run_git_status(&self, args: &[&str]) -> Result<(i32, Vec<u8>, String), GitError> {
        let rendered: Vec<String> = args.iter().map(ToString::to_string).collect();
        let out = Self::base_command()
            .args(args)
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| GitError::Command {
                args: rendered,
                code: None,
                stderr: e.to_string(),
            })?;
        Ok((
            out.status.code().unwrap_or(-1),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }

    /// The commit id `HEAD` currently points at.
    pub fn head_oid(&self) -> Result<GitOid, GitError> {
        self.parse_oid(&self.run_git(&["rev-parse", "HEAD"])?)
    }

    /// The tree id of the given commit.
    pub fn tree_oid(&self, commit: &GitOid) -> Result<GitOid, GitError> {
        let spec = format!("{}^{{tree}}", commit.as_str());
        self.parse_oid(&self.run_git(&["rev-parse", &spec])?)
    }

    /// Capture the authoritative canonical state: head commit, its tree, and
    /// the capture timestamp (§5.1).
    pub fn capture_canonical(&self) -> Result<CanonicalState, GitError> {
        let commit = self.head_oid()?;
        let tree = self.tree_oid(&commit)?;
        Ok(CanonicalState {
            commit,
            tree,
            captured_at: SystemTime::now(),
        })
    }

    /// Create an isolated speculative workspace for `session_id` as a detached
    /// `git worktree` checked out at `base` (§9). The workspace lives at
    /// `.doubledoor/sessions/<id>/workspace` under the repository root.
    ///
    /// Isolation failure semantics: if anything fails after the session
    /// directory has been created, the directory tree is removed best-effort
    /// and the error is still returned — a partially-initialized workspace is
    /// never presented as usable. The canonical state is never touched.
    pub fn create_workspace(
        &self,
        session_id: &DoorSessionId,
        base: &GitOid,
    ) -> Result<WorkspaceRef, GitError> {
        let session_dir = self.session_dir(session_id)?;
        fs::create_dir_all(&session_dir).map_err(|e| {
            GitError::UnexpectedOutput(format!(
                "cannot create session dir {}: {e}",
                session_dir.display()
            ))
        })?;

        let outcome = self.materialize_workspace(&session_dir, base);
        match outcome {
            Ok(ws) => Ok(ws),
            Err(err) => {
                // §9: never leave a partially-initialized workspace behind.
                if let Err(cleanup_err) = fs::remove_dir_all(&session_dir) {
                    warn!(
                        "failed to clean up session dir {} after isolation failure: {cleanup_err}",
                        session_dir.display()
                    );
                }
                Err(err)
            }
        }
    }

    /// Remove the workspace registered for `session_id`. If the path is still
    /// registered as a worktree it is deregistered with
    /// `git worktree remove --force`; either way the directory tree is removed
    /// best-effort, so calling this for an already-missing workspace is a no-op.
    pub fn remove_workspace(&self, session_id: &DoorSessionId) -> Result<(), GitError> {
        let session_dir = self.session_dir(session_id)?;
        let ws = session_dir.join(WORKSPACE_DIR_NAME);

        if self
            .registered_worktrees()?
            .iter()
            .any(|p| same_path(p, &ws))
        {
            let ws_arg = path_arg(&ws)?;
            self.run_git(&["worktree", "remove", "--force", ws_arg])?;
        }

        for dir in [&ws, &session_dir] {
            if dir.exists() {
                if let Err(e) = fs::remove_dir_all(dir) {
                    warn!("best-effort removal of {} failed: {e}", dir.display());
                }
            }
        }
        Ok(())
    }

    /// Remove the workspace recorded in session metadata, after validating
    /// that it really is this session's canonical workspace
    /// (`.doubledoor/sessions/<id>/workspace`). Doubledoor always
    /// materializes workspaces at that location, so a recorded path that
    /// resolves anywhere else means stale or tampered metadata — acting on it
    /// would delete an arbitrary directory (§27 path hygiene). Returns
    /// `PathEscape` in that case. Unlike [`Git::remove_workspace`], the
    /// session directory itself (metadata + journal) is left in place, and a
    /// workspace that is already gone is a no-op.
    pub fn remove_recorded_workspace(
        &self,
        session_id: &DoorSessionId,
        recorded: &Path,
    ) -> Result<(), GitError> {
        let session_dir = self.session_dir(session_id)?;
        let ws = session_dir.join(WORKSPACE_DIR_NAME);
        let actual = match recorded.canonicalize() {
            Ok(p) => p,
            Err(_) => return Ok(()), // nothing on disk to remove
        };
        let expected = ws
            .canonicalize()
            .map_err(|_| GitError::PathEscape(recorded.to_path_buf()))?;
        if actual != expected {
            return Err(GitError::PathEscape(recorded.to_path_buf()));
        }

        if self
            .registered_worktrees()?
            .iter()
            .any(|p| same_path(p, &ws))
        {
            let ws_arg = path_arg(&ws)?;
            self.run_git(&["worktree", "remove", "--force", ws_arg])?;
        }
        if ws.exists() {
            fs::remove_dir_all(&ws).map_err(|e| {
                GitError::UnexpectedOutput(format!("cannot remove workspace {}: {e}", ws.display()))
            })?;
        }
        Ok(())
    }

    /// Absolute paths of every worktree registered in this repository, parsed
    /// from `git worktree list --porcelain`.
    pub fn registered_worktrees(&self) -> Result<Vec<PathBuf>, GitError> {
        let out = self.run_git(&["worktree", "list", "--porcelain"])?;
        Ok(out
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect())
    }

    /// The `.doubledoor/sessions/<id>` directory for a session, validated
    /// against traversal (§27). `DoorSessionId` renders as `dd_<uuid>` and is
    /// filesystem-safe by construction; this check is defense in depth, and
    /// the composed path is additionally checked for `..` / prefix components.
    fn session_dir(&self, session_id: &DoorSessionId) -> Result<PathBuf, GitError> {
        let id = session_id.as_str();
        if id.is_empty() || id.contains(['/', '\\', '\0']) || id.contains("..") {
            return Err(GitError::PathEscape(PathBuf::from(id)));
        }
        let dir = self.repo_root.join(SESSIONS_DIR).join(&id);
        if dir
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(GitError::PathEscape(dir));
        }
        Ok(dir)
    }

    fn materialize_workspace(
        &self,
        session_dir: &Path,
        base: &GitOid,
    ) -> Result<WorkspaceRef, GitError> {
        // Defense in depth (§27): refuse to act through a path that resolves
        // outside the repository root (e.g. a symlinked ancestor).
        self.assert_within_root(session_dir)?;
        let ws = session_dir.join(WORKSPACE_DIR_NAME);
        let ws_arg = path_arg(&ws)?;
        self.run_git(&["worktree", "add", "--detach", ws_arg, base.as_str()])?;
        Ok(WorkspaceRef { path: ws })
    }

    fn assert_within_root(&self, dir: &Path) -> Result<(), GitError> {
        let root = self.repo_root.canonicalize().map_err(|e| {
            GitError::UnexpectedOutput(format!(
                "cannot canonicalize {}: {e}",
                self.repo_root.display()
            ))
        })?;
        let cand = dir.canonicalize().map_err(|e| {
            GitError::UnexpectedOutput(format!("cannot canonicalize {}: {e}", dir.display()))
        })?;
        if !cand.starts_with(&root) {
            return Err(GitError::PathEscape(dir.to_path_buf()));
        }
        Ok(())
    }

    fn parse_oid(&self, raw: &str) -> Result<GitOid, GitError> {
        GitOid::parse(raw)
            .map_err(|_| GitError::UnexpectedOutput(format!("not a git object id: {raw}")))
    }
}

/// Compare two paths, canonicalizing both when possible so symlinked
/// ancestors do not cause spurious mismatches.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// A path as a lossless `&str` argument, or [`GitError::PathEscape`] when it
/// is not valid UTF-8 (and therefore cannot be safely passed to `git`).
fn path_arg(path: &Path) -> Result<&str, GitError> {
    path.to_str()
        .ok_or_else(|| GitError::PathEscape(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A freshly initialized repository with one commit, living in a unique
    /// subdirectory of the system temp dir. Removed on drop.
    struct TempRepo {
        root: PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "doubledoor-git-test-{}-{nanos}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            git_at(&root, &["init"]);
            git_at(&root, &["config", "user.email", "test@doubledoor.dev"]);
            git_at(&root, &["config", "user.name", "Doubledoor Test"]);
            fs::write(root.join("file.txt"), "hello\n").unwrap();
            git_at(&root, &["add", "file.txt"]);
            git_at(&root, &["commit", "-m", "initial"]);
            TempRepo { root }
        }

        fn git(&self) -> Git {
            Git::new(self.root.clone())
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Raw git invocation (no Doubledoor machinery) for test assertions.
    fn git_at(dir: &Path, args: &[&str]) -> String {
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

    fn workspace_path(repo: &TempRepo, id: &DoorSessionId) -> PathBuf {
        repo.root
            .join(SESSIONS_DIR)
            .join(id.as_str())
            .join(WORKSPACE_DIR_NAME)
    }

    #[test]
    fn capture_canonical_roundtrips_rev_parse() {
        let repo = TempRepo::new();
        let git = repo.git();
        let canonical = git.capture_canonical().unwrap();
        assert_eq!(canonical.commit, git.head_oid().unwrap());
        assert_eq!(canonical.tree, git.tree_oid(&canonical.commit).unwrap());
        assert!(canonical.captured_at <= SystemTime::now());
    }

    #[test]
    fn discover_finds_toplevel_from_subdirectory() {
        let repo = TempRepo::new();
        let nested = repo.root.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let git = Git::discover(&nested).unwrap();
        assert_eq!(
            git.repo_root().canonicalize().unwrap(),
            repo.root.canonicalize().unwrap()
        );
    }

    #[test]
    fn discover_outside_a_repo_is_typed_error() {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("doubledoor-git-norepo-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let err = Git::discover(&dir).unwrap_err();
        assert!(matches!(err, GitError::NotARepository(_)));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_workspace_checks_out_base_detached() {
        let repo = TempRepo::new();
        let git = repo.git();
        let base = git.head_oid().unwrap();
        let id = DoorSessionId::new();

        let ws = git.create_workspace(&id, &base).unwrap();
        assert_eq!(ws.path, workspace_path(&repo, &id));

        // The worktree is checked out at the base commit.
        assert_eq!(git_at(&ws.path, &["rev-parse", "HEAD"]), base.as_str());
        // The checkout is detached: no branch symbolic-ref for HEAD.
        let out = Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(&ws.path)
            .output()
            .unwrap();
        assert!(!out.status.success(), "workspace HEAD must be detached");
        // A worktree carries a `.git` file (not a directory) pointing back at
        // the main repository's administrative area.
        let gitfile = fs::read_to_string(ws.path.join(".git")).unwrap();
        assert!(gitfile.starts_with("gitdir:"));
        // Checked-out content is present.
        assert_eq!(
            fs::read_to_string(ws.path.join("file.txt")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn registered_worktrees_lists_created_workspace() {
        let repo = TempRepo::new();
        let git = repo.git();
        let base = git.head_oid().unwrap();
        let id = DoorSessionId::new();
        let ws = git.create_workspace(&id, &base).unwrap();

        let list = git.registered_worktrees().unwrap();
        assert!(
            list.iter().any(|p| same_path(p, &ws.path)),
            "worktree list should contain {}",
            ws.path.display()
        );
    }

    #[test]
    fn remove_workspace_deregisters_and_deletes() {
        let repo = TempRepo::new();
        let git = repo.git();
        let base = git.head_oid().unwrap();
        let id = DoorSessionId::new();
        let ws = git.create_workspace(&id, &base).unwrap();
        assert!(ws.path.exists());

        git.remove_workspace(&id).unwrap();

        let list = git.registered_worktrees().unwrap();
        assert!(
            !list.iter().any(|p| same_path(p, &ws.path)),
            "worktree list should no longer contain {}",
            ws.path.display()
        );
        assert!(!ws.path.exists());
        assert!(!repo.root.join(SESSIONS_DIR).join(id.as_str()).exists());
    }

    #[test]
    fn remove_workspace_tolerates_missing() {
        let repo = TempRepo::new();
        let git = repo.git();
        let id = DoorSessionId::new();
        git.remove_workspace(&id).unwrap();
        git.remove_workspace(&id).unwrap();
    }

    #[test]
    fn remove_recorded_workspace_removes_canonical_but_keeps_session_dir() {
        let repo = TempRepo::new();
        let git = repo.git();
        let base = git.head_oid().unwrap();
        let id = DoorSessionId::new();
        let ws = git.create_workspace(&id, &base).unwrap();
        assert!(ws.path.exists());

        git.remove_recorded_workspace(&id, &ws.path).unwrap();

        let list = git.registered_worktrees().unwrap();
        assert!(
            !list.iter().any(|p| same_path(p, &ws.path)),
            "worktree list should no longer contain {}",
            ws.path.display()
        );
        assert!(!ws.path.exists());
        // Unlike remove_workspace, the session dir (metadata + journal) stays.
        assert!(repo.root.join(SESSIONS_DIR).join(id.as_str()).exists());
    }

    #[test]
    fn remove_recorded_workspace_refuses_tampered_metadata_path() {
        let repo = TempRepo::new();
        let git = repo.git();
        let base = git.head_oid().unwrap();
        let id = DoorSessionId::new();
        let ws = git.create_workspace(&id, &base).unwrap();

        // Tampered session metadata points the workspace at an unrelated
        // existing directory; removal must refuse and leave it untouched.
        let tampered = repo.root.join("tampered-target");
        fs::create_dir_all(&tampered).unwrap();
        fs::write(tampered.join("precious.txt"), "do not delete\n").unwrap();

        let err = git.remove_recorded_workspace(&id, &tampered).unwrap_err();
        assert!(
            matches!(err, GitError::PathEscape(_)),
            "expected PathEscape, got {err:?}"
        );
        assert_eq!(
            fs::read_to_string(tampered.join("precious.txt")).unwrap(),
            "do not delete\n"
        );

        // A recorded path that no longer exists is a no-op, not an error.
        let gone = workspace_path(&repo, &DoorSessionId::new());
        git.remove_recorded_workspace(&id, &gone).unwrap();
        // ...and the real workspace is untouched by both calls.
        assert!(ws.path.exists());
        git.remove_workspace(&id).unwrap();
    }

    #[test]
    fn internal_ops_never_fire_hooks() {
        let repo = TempRepo::new();
        let git = repo.git();

        // A post-checkout hook that records every firing in a marker file.
        let marker = repo.root.join("hook_marker.txt");
        let hook = repo.root.join(".git").join("hooks").join("post-checkout");
        fs::write(
            &hook,
            format!("#!/bin/sh\necho fired >> {}\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let base = git.head_oid().unwrap();
        let id = DoorSessionId::new();
        let ws = git.create_workspace(&id, &base).unwrap();
        assert!(
            !marker.exists(),
            "post-checkout hook must not fire during internal operations (§27)"
        );

        // Control: the same operation through raw git (no hook suppression)
        // does fire the hook, proving suppression is what kept it silent.
        let raw_ws = repo.root.join("raw-workspace");
        git_at(
            &repo.root,
            &[
                "worktree",
                "add",
                "--detach",
                raw_ws.to_str().unwrap(),
                base.as_str(),
            ],
        );
        assert!(marker.exists(), "control run should have fired the hook");
        git_at(
            &repo.root,
            &["worktree", "remove", "--force", raw_ws.to_str().unwrap()],
        );

        drop(ws);
    }

    #[test]
    fn failed_isolation_leaves_no_partial_workspace() {
        let repo = TempRepo::new();
        let git = repo.git();
        let id = DoorSessionId::new();
        // Not a valid commit: worktree add must fail.
        let bogus = GitOid::parse(&"f".repeat(40)).unwrap();

        let err = git.create_workspace(&id, &bogus).unwrap_err();
        assert!(matches!(err, GitError::Command { .. }));
        // The session directory must be cleaned up, not left half-built (§9).
        assert!(!repo.root.join(SESSIONS_DIR).join(id.as_str()).exists());
        // Canonical state is untouched.
        assert!(git.capture_canonical().is_ok());
    }

    #[test]
    fn command_error_carries_argv_code_and_stderr() {
        let repo = TempRepo::new();
        let git = repo.git();
        let err = git.run_git(&["rev-parse", "does-not-exist"]).unwrap_err();
        match err {
            GitError::Command { args, code, stderr } => {
                assert_eq!(
                    args,
                    vec!["rev-parse".to_string(), "does-not-exist".to_string()]
                );
                assert!(code.is_some_and(|c| c != 0));
                assert!(!stderr.is_empty());
            }
            other => panic!("expected GitError::Command, got {other:?}"),
        }
    }
}
