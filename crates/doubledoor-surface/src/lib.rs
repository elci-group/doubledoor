//! Mutation surfaces (§5.3, §10, §11): file-level (Phase II) and
//! Rust-semantic (Phase III).
//!
//! A *surface* is a unit of mutation contention. Every tracked path is a
//! [`FileSurface`] whose stable identifier is `file:<repo-relative path>`.
//! Phase III adds finer surfaces behind the same [`SurfaceId`] type:
//!
//! - [`rust`] — `syn`-based symbol extraction: items, members, normalized
//!   signatures, body hashes, and references (`rust:<crate>::<path>` ids);
//! - [`cratemap`] — file path → crate/module path from the Cargo manifests;
//! - [`cargo`] — structural `Cargo.toml` diffs (`cargo:<manifest>#<key>`);
//! - [`semantic`] — symbol-level commit deltas, a session's derived
//!   footprint, and the reference relations between them.
//!
//! Pure set logic ([`normalize_path`], [`parse_name_status`],
//! [`SurfaceDelta`]) is free of Git I/O so it is unit-testable without a
//! repository; only [`SurfaceIndex::build`] and
//! [`SurfaceDelta::between_commits`] touch Git, through the
//! `doubledoor-git` choke point.

#![forbid(unsafe_code)]

pub mod cargo;
pub mod cratemap;
mod error;
pub mod rust;
pub mod semantic;

pub use error::SurfaceError;

use std::collections::BTreeMap;
use std::path::{Component, Path};

use doubledoor_core::{GitOid, SurfaceId, SurfaceSet};
use doubledoor_git::Git;

/// The stable id prefix for file-level surfaces.
const FILE_ID_PREFIX: &str = "file:";

/// A file-level mutation surface (§5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSurface {
    /// Repo-relative path, normalized (§27 path hygiene): no leading `./`,
    /// no `.` components, never absolute, never containing `..`.
    pub path: String,
    pub id: SurfaceId,
}

impl FileSurface {
    /// Build a surface from a repo-relative path. The id is derived
    /// deterministically as `file:<normalized path>`, so the same path always
    /// yields the same surface — across builds and across sessions.
    pub fn new(path: impl Into<String>) -> Result<Self, SurfaceError> {
        let path = normalize_path(&path.into())?;
        let id = SurfaceId::new(format!("{FILE_ID_PREFIX}{path}"))?;
        Ok(FileSurface { path, id })
    }
}

/// Validate and normalize a repo-relative path (§27 path hygiene): reject
/// absolute paths and any `..` component, collapse redundant `.`/empty
/// segments and leading `./`, and require a non-empty result.
pub fn normalize_path(raw: &str) -> Result<String, SurfaceError> {
    if raw.is_empty() {
        return Err(SurfaceError::InvalidPath(raw.to_string()));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(SurfaceError::InvalidPath(raw.to_string()));
    }
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SurfaceError::InvalidPath(raw.to_string()));
            }
        }
    }
    if parts.is_empty() {
        return Err(SurfaceError::InvalidPath(raw.to_string()));
    }
    Ok(parts.join("/"))
}

/// Resolves a repo-relative path to a surface. [`SurfaceIndex`] implements
/// this for file-level granularity; a Phase III symbol-level resolver will
/// implement the same trait so ghost classification keeps working unchanged
/// when surfaces become finer-grained.
pub trait SurfaceResolver {
    fn surface_for_path(&self, path: &str) -> Option<SurfaceId>;
}

/// The set of file-level surfaces in a repository, built from `git ls-files`.
#[derive(Debug, Clone, Default)]
pub struct SurfaceIndex {
    by_path: BTreeMap<String, SurfaceId>,
    by_id: BTreeMap<SurfaceId, String>,
}

impl SurfaceIndex {
    /// Index every tracked path at the repository's current `HEAD`.
    pub fn build(git: &Git) -> Result<Self, SurfaceError> {
        let out = git.run_git(&["ls-files"])?;
        let mut index = SurfaceIndex::default();
        for line in out.lines() {
            if line.is_empty() {
                continue;
            }
            let surface = FileSurface::new(line)?;
            index
                .by_path
                .insert(surface.path.clone(), surface.id.clone());
            index.by_id.insert(surface.id, surface.path);
        }
        Ok(index)
    }

    /// The surface for a repo-relative path, if the path is tracked. The path
    /// is normalized first, so `./src/x.rs` and `src/x.rs` resolve alike.
    pub fn surface_for_path(&self, path: &str) -> Option<SurfaceId> {
        let normalized = normalize_path(path).ok()?;
        self.by_path.get(&normalized).cloned()
    }

    /// The repo-relative path a surface was minted from, if indexed.
    pub fn path_for_surface(&self, id: &SurfaceId) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }

    /// Every indexed surface as `(path, id)` pairs in deterministic
    /// (path-sorted) order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SurfaceId)> {
        self.by_path.iter().map(|(path, id)| (path.as_str(), id))
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

impl SurfaceResolver for SurfaceIndex {
    fn surface_for_path(&self, path: &str) -> Option<SurfaceId> {
        SurfaceIndex::surface_for_path(self, path)
    }
}

/// The set of surfaces whose content differs between two commits, derived
/// from `git diff-tree --no-commit-id --name-status -r <a> <b>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceDelta {
    pub added: SurfaceSet,
    pub removed: SurfaceSet,
    pub modified: SurfaceSet,
}

impl SurfaceDelta {
    /// Compute the delta between two commits. Renames are reported by Git as
    /// `R<score>` lines and are treated as remove+add at this level, matching
    /// the directive's Phase II granularity.
    pub fn between_commits(git: &Git, from: &GitOid, to: &GitOid) -> Result<Self, SurfaceError> {
        let out = git.run_git(&[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            from.as_str(),
            to.as_str(),
        ])?;
        parse_name_status(&out)
    }

    /// Every touched surface: added ∪ removed ∪ modified.
    pub fn affected(&self) -> SurfaceSet {
        self.added.union(&self.removed).union(&self.modified)
    }

    /// The subset of affected surfaces that intersect `other`.
    pub fn intersect(&self, other: &SurfaceSet) -> SurfaceSet {
        self.affected().intersection(other)
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

/// Pure parse of `git diff-tree --name-status -r` body lines (the
/// `--no-commit-id` form). Each line is `<status>\t<path>` — or
/// `<status>\t<old>\t<new>` for renames. Supported letters: `A`, `M`, `D`,
/// `T` (type change, treated as modified), and `R` (treated as remove+add).
/// Any other letter or a malformed line is a typed [`SurfaceError`]: surface
/// arithmetic must never silently drop a change.
pub fn parse_name_status(output: &str) -> Result<SurfaceDelta, SurfaceError> {
    let mut delta = SurfaceDelta::default();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let status = fields.next().unwrap_or_default();
        let letter = &status[..status.len().min(1)];
        match letter {
            "A" => {
                delta
                    .added
                    .insert(FileSurface::new(next_path(&mut fields, line)?)?.id);
            }
            "M" | "T" => {
                delta
                    .modified
                    .insert(FileSurface::new(next_path(&mut fields, line)?)?.id);
            }
            "D" => {
                delta
                    .removed
                    .insert(FileSurface::new(next_path(&mut fields, line)?)?.id);
            }
            "R" => {
                let old = next_path(&mut fields, line)?;
                let new = next_path(&mut fields, line)?;
                delta.removed.insert(FileSurface::new(old)?.id);
                delta.added.insert(FileSurface::new(new)?.id);
            }
            other => {
                return Err(SurfaceError::UnknownStatus(other.to_string()));
            }
        }
    }
    Ok(delta)
}

/// The next non-empty tab-separated path field of a name-status line.
fn next_path<'a>(
    fields: &mut std::str::Split<'a, char>,
    line: &str,
) -> Result<&'a str, SurfaceError> {
    fields
        .next()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| SurfaceError::MalformedLine(line.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(path: &str) -> SurfaceId {
        FileSurface::new(path).unwrap().id
    }

    // ------------------------------------------------------------------
    // Path hygiene (pure)
    // ------------------------------------------------------------------

    #[test]
    fn normalize_strips_dot_segments() {
        assert_eq!(normalize_path("./src/x.rs").unwrap(), "src/x.rs");
        assert_eq!(normalize_path("src/./x.rs").unwrap(), "src/x.rs");
        assert_eq!(normalize_path("src//x.rs").unwrap(), "src/x.rs");
        assert_eq!(normalize_path("src/x.rs").unwrap(), "src/x.rs");
    }

    #[test]
    fn normalize_rejects_traversal_and_absolute() {
        assert!(normalize_path("../x.rs").is_err());
        assert!(normalize_path("src/../x.rs").is_err());
        assert!(normalize_path("/abs/path.rs").is_err());
        assert!(normalize_path("").is_err());
        assert!(normalize_path(".").is_err());
    }

    #[test]
    fn file_surface_id_is_stable() {
        let a = FileSurface::new("./src/x.rs").unwrap();
        let b = FileSurface::new("src/x.rs").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.id.as_str(), "file:src/x.rs");
    }

    // ------------------------------------------------------------------
    // name-status parsing (pure)
    // ------------------------------------------------------------------

    #[test]
    fn parse_name_status_classifies_letters() {
        let out = "A\tsrc/new.rs\nM\tsrc/changed.rs\nD\tsrc/gone.rs\nT\tsrc/mode.sh\n";
        let delta = parse_name_status(out).unwrap();
        assert_eq!(
            delta.added.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            ["file:src/new.rs"]
        );
        assert_eq!(
            delta
                .modified
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            ["file:src/changed.rs", "file:src/mode.sh"]
        );
        assert_eq!(
            delta.removed.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            ["file:src/gone.rs"]
        );
        assert_eq!(delta.affected().len(), 4);
    }

    #[test]
    fn parse_name_status_treats_renames_as_remove_and_add() {
        let out = "R087\tsrc/old.rs\tsrc/new.rs\n";
        let delta = parse_name_status(out).unwrap();
        assert!(delta.removed.contains(&sid("src/old.rs")));
        assert!(delta.added.contains(&sid("src/new.rs")));
        assert!(delta.modified.is_empty());
    }

    #[test]
    fn parse_name_status_rejects_unknown_letters_and_bad_lines() {
        assert!(parse_name_status("X\tsrc/x.rs\n").is_err());
        assert!(parse_name_status("M\n").is_err());
        assert!(parse_name_status("R100\tonly-one-path\n").is_err());
        assert!(parse_name_status("M\t../escape.rs\n").is_err());
    }

    #[test]
    fn delta_intersect_selects_touched_subset() {
        let out = "A\tsrc/a.rs\nM\tsrc/b.rs\nD\tsrc/c.rs\n";
        let delta = parse_name_status(out).unwrap();
        let probe: SurfaceSet = [sid("src/b.rs"), sid("src/untouched.rs")]
            .into_iter()
            .collect();
        let hit = delta.intersect(&probe);
        assert_eq!(hit.len(), 1);
        assert!(hit.contains(&sid("src/b.rs")));
    }
}
