//! Git-backed surface tests against real repositories in tempdirs (§36).

use std::path::Path;
use std::process::Command;

use doubledoor_core::GitOid;
use doubledoor_git::Git;
use doubledoor_surface::{FileSurface, SurfaceDelta, SurfaceIndex};

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

    fn commit_all(&self, message: &str) -> GitOid {
        raw(self.dir.path(), &["add", "-A"]);
        raw(self.dir.path(), &["commit", "-m", message]);
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

#[test]
fn index_builds_from_ls_files() {
    let repo = TempRepo::new();
    repo.write("src/lib.rs", "pub fn a() {}\n");
    repo.write("docs/readme.md", "# hi\n");
    repo.commit_all("add files");

    let index = SurfaceIndex::build(&repo.git()).unwrap();
    assert_eq!(index.len(), 2);
    let lib = index.surface_for_path("src/lib.rs").unwrap();
    assert_eq!(lib.as_str(), "file:src/lib.rs");
    assert_eq!(index.path_for_surface(&lib), Some("src/lib.rs"));
    assert_eq!(index.surface_for_path("./src/lib.rs"), Some(lib));
    assert!(index.surface_for_path("src/missing.rs").is_none());
}

#[test]
fn delta_between_commits_tracks_added_modified_deleted() {
    let repo = TempRepo::new();
    repo.write("keep.rs", "k\n");
    repo.write("change.rs", "v1\n");
    repo.write("gone.rs", "g\n");
    let base = repo.commit_all("seed");

    repo.write("change.rs", "v2\n");
    std::fs::remove_file(repo.dir.path().join("gone.rs")).unwrap();
    repo.write("fresh.rs", "f\n");
    let head = repo.commit_all("mutate");

    let delta = SurfaceDelta::between_commits(&repo.git(), &base, &head).unwrap();
    assert!(delta
        .added
        .contains(&FileSurface::new("fresh.rs").unwrap().id));
    assert!(delta
        .modified
        .contains(&FileSurface::new("change.rs").unwrap().id));
    assert!(delta
        .removed
        .contains(&FileSurface::new("gone.rs").unwrap().id));
    assert!(!delta
        .affected()
        .contains(&FileSurface::new("keep.rs").unwrap().id));
    assert!(!delta.is_empty());
}

#[test]
fn rename_is_reported_as_remove_and_add() {
    let repo = TempRepo::new();
    repo.write("before.rs", "b\n");
    let base = repo.commit_all("seed");

    std::fs::rename(
        repo.dir.path().join("before.rs"),
        repo.dir.path().join("after.rs"),
    )
    .unwrap();
    repo.commit_all("rename with detection");

    // diff-tree without rename detection: plain D + A.
    let delta =
        SurfaceDelta::between_commits(&repo.git(), &base, &repo.git().head_oid().unwrap()).unwrap();
    assert!(delta
        .removed
        .contains(&FileSurface::new("before.rs").unwrap().id));
    assert!(delta
        .added
        .contains(&FileSurface::new("after.rs").unwrap().id));
}
