//! Semantic surfaces (§10, §11, §47 Phase III): symbol-level deltas of a
//! commit, the symbol-level footprint of a session's speculative work, and
//! the relation between the two.
//!
//! - [`semantic_delta`] / [`dependency_delta`] — what a commit changed, as
//!   symbol changes (added / removed / signature / visibility / body) with
//!   their API impact, and as structural Cargo changes;
//! - [`analyze_workspace`] — what a session's work modified and what it
//!   relies on (its references, struct literals, exhaustive matches, trait
//!   impls, external crates), from its workspace versus its base commit;
//! - [`SessionSemantics::relations`] — how a session's references relate to
//!   one changed symbol. This is what lets Doubledoor see that a signature
//!   change in `router.rs` breaks a caller in `handler.rs` that Git's line
//!   analysis considers untouched.
//!
//! All evidence here is syntactic (AST level, §17): names resolve through
//! `use` declarations and path prefixes, not the compiler.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use doubledoor_core::{GitOid, SurfaceId};
use doubledoor_git::Git;

use crate::cargo::{diff_manifest, DependencyDelta};
use crate::cratemap::{manifest_dir, CrateMap};
use crate::error::SurfaceError;
use crate::normalize_path;
use crate::rust::{
    analyze_source, FileSymbols, RefTarget, Reference, Symbol, SymbolKind, SymbolVisibility,
};

/// Manifest file name.
const MANIFEST: &str = "Cargo.toml";

/// How a symbol changed between two versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Removed,
    SignatureChanged {
        before: String,
        after: String,
    },
    VisibilityChanged {
        before: SymbolVisibility,
        after: SymbolVisibility,
    },
    /// Implementation changed; signature and visibility did not.
    BodyChanged,
}

/// The API consequence of a symbol change, independent of who uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ApiImpact {
    /// Behaviour may differ; the interface is identical.
    Behavioral,
    /// New interface; existing uses keep compiling except in the contextual
    /// cases (struct literals, exhaustive matches, trait impls).
    Additive,
    /// Existing uses may stop compiling.
    Breaking,
}

/// One symbol-level change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolChange {
    pub symbol: SurfaceId,
    pub path: Vec<String>,
    pub kind: SymbolKind,
    /// Visibility after the change (before, for removals).
    pub visibility: SymbolVisibility,
    pub change: ChangeKind,
    pub impact: ApiImpact,
    /// For trait items: a default body exists after the change.
    pub has_default: bool,
    pub file: String,
    pub line: usize,
}

impl SymbolChange {
    pub fn qualified(&self) -> String {
        crate::rust::qualify(&self.path, self.kind)
    }

    /// Short human description: `signature changed: fn a() -> X → fn a() -> Y`.
    pub fn describe(&self) -> String {
        match &self.change {
            ChangeKind::Added => "added".to_string(),
            ChangeKind::Removed => "removed".to_string(),
            ChangeKind::SignatureChanged { before, after } => {
                format!(
                    "signature changed: `{}` → `{}`",
                    pretty_tokens(before),
                    pretty_tokens(after)
                )
            }
            ChangeKind::VisibilityChanged { before, after } => {
                format!("visibility changed: {before:?} → {after:?}")
            }
            ChangeKind::BodyChanged => "implementation changed (signature unchanged)".to_string(),
        }
    }
}

/// Display form of a normalized token string (`fn a (x : & str) -> Vec < u8 >`
/// → `fn a(x: &str) -> Vec<u8>`). Presentation only: stored signatures and
/// comparisons always use the canonical token form.
pub fn pretty_tokens(s: &str) -> String {
    let mut out = s.to_string();
    for (from, to) in [
        (" :: ", "::"),
        (" ( ", "("),
        (" (", "("),
        ("( ", "("),
        (" )", ")"),
        (" , ", ", "),
        (" ,", ","),
        (" < ", "<"),
        ("< ", "<"),
        (" <", "<"),
        (" >", ">"),
        (" : ", ": "),
        ("& ", "&"),
        (" ;", ";"),
    ] {
        out = out.replace(from, to);
    }
    out
}

/// A Rust file that could not be analyzed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnparsedFile {
    pub path: String,
    /// Which version: `before`, `after`, `workspace`.
    pub side: String,
    pub reason: String,
}

/// Symbol-level delta of one commit (§12 `SemanticDelta`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticDelta {
    pub changes: Vec<SymbolChange>,
    pub analyzed_files: Vec<String>,
    pub unparsed: Vec<UnparsedFile>,
}

/// The public-API view of a delta (§12 `ApiDelta`): changes to `pub` items.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiDelta {
    pub breaking: Vec<SurfaceId>,
    pub additive: Vec<SurfaceId>,
}

impl SemanticDelta {
    pub fn api(&self) -> ApiDelta {
        let mut api = ApiDelta::default();
        for c in &self.changes {
            if c.visibility != SymbolVisibility::Public
                && !matches!(
                    c.change,
                    ChangeKind::VisibilityChanged {
                        before: SymbolVisibility::Public,
                        ..
                    }
                )
            {
                continue;
            }
            match c.impact {
                ApiImpact::Breaking => api.breaking.push(c.symbol.clone()),
                ApiImpact::Additive => api.additive.push(c.symbol.clone()),
                ApiImpact::Behavioral => {}
            }
        }
        api
    }
}

/// Diff two symbol tables (`(file, symbol)` lists) into changes.
pub fn diff_symbols(before: &[(String, Symbol)], after: &[(String, Symbol)]) -> Vec<SymbolChange> {
    type Key = (Vec<String>, SymbolKind);
    let index = |v: &[(String, Symbol)]| -> BTreeMap<Key, (String, Symbol)> {
        v.iter()
            .map(|(f, s)| ((s.path.clone(), s.kind), (f.clone(), s.clone())))
            .collect()
    };
    let b = index(before);
    let a = index(after);
    let mut out = Vec::new();
    for (key, (file, sym)) in &a {
        let change = match b.get(key) {
            None => ChangeKind::Added,
            Some((_, old)) if old.signature != sym.signature => ChangeKind::SignatureChanged {
                before: old.signature.clone(),
                after: sym.signature.clone(),
            },
            Some((_, old)) if old.visibility != sym.visibility => ChangeKind::VisibilityChanged {
                before: old.visibility,
                after: sym.visibility,
            },
            // A trait item gaining/losing its default body is an API change
            // (checked before the body hash, which also changes).
            Some((_, old)) if old.has_default != sym.has_default => ChangeKind::SignatureChanged {
                before: format!("{} (default: {})", old.signature, old.has_default),
                after: format!("{} (default: {})", sym.signature, sym.has_default),
            },
            Some((_, old)) if old.body_hash != sym.body_hash => ChangeKind::BodyChanged,
            Some(_) => continue,
        };
        out.push(make_change(file, sym, change));
    }
    for (key, (file, sym)) in &b {
        if !a.contains_key(key) {
            out.push(make_change(file, sym, ChangeKind::Removed));
        }
    }
    out.sort_by(|x, y| x.symbol.cmp(&y.symbol).then(x.kind.cmp(&y.kind)));
    out
}

fn make_change(file: &str, sym: &Symbol, change: ChangeKind) -> SymbolChange {
    let impact = match &change {
        ChangeKind::Removed | ChangeKind::SignatureChanged { .. } => ApiImpact::Breaking,
        ChangeKind::VisibilityChanged { before, after } if after < before => ApiImpact::Breaking,
        ChangeKind::VisibilityChanged { .. } | ChangeKind::Added => ApiImpact::Additive,
        ChangeKind::BodyChanged => ApiImpact::Behavioral,
    };
    SymbolChange {
        symbol: sym.surface_id(),
        path: sym.path.clone(),
        kind: sym.kind,
        visibility: sym.visibility,
        change,
        impact,
        has_default: sym.has_default,
        file: file.to_string(),
        line: sym.line,
    }
}

// ---------------------------------------------------------------------------
// Git-backed loading
// ---------------------------------------------------------------------------

/// `git show <commit>:<path>` as UTF-8, `None` when absent at that commit.
fn file_at(git: &Git, commit: &GitOid, path: &str) -> Option<String> {
    let spec = format!("{}:{path}", commit.as_str());
    git.run_git_bytes(&["show", &spec])
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Every manifest in the tree of `commit`.
pub fn crate_map_at(git: &Git, commit: &GitOid) -> Result<CrateMap, SurfaceError> {
    let listing = git.run_git(&["ls-tree", "-r", "--name-only", commit.as_str()])?;
    let manifests: Vec<(String, String)> = listing
        .lines()
        .filter(|p| p.rsplit('/').next() == Some(MANIFEST))
        .filter_map(|p| file_at(git, commit, p).map(|c| (p.to_string(), c)))
        .collect();
    Ok(CrateMap::from_manifests(manifests))
}

fn symbols_of(
    map: &CrateMap,
    path: &str,
    source: Option<String>,
    side: &str,
    unparsed: &mut Vec<UnparsedFile>,
) -> Option<FileSymbols> {
    let module = map.module_of(path)?;
    let source = source?;
    match analyze_source(&source, &module) {
        Ok(fs) => Some(fs),
        Err(e) => {
            unparsed.push(UnparsedFile {
                path: path.to_string(),
                side: side.to_string(),
                reason: e.to_string(),
            });
            None
        }
    }
}

fn tagged(path: &str, fs: Option<&FileSymbols>) -> Vec<(String, Symbol)> {
    fs.map(|f| {
        f.symbols
            .iter()
            .map(|s| (path.to_string(), s.clone()))
            .collect()
    })
    .unwrap_or_default()
}

/// Symbol-level delta of `parent → commit` over the changed `.rs` paths.
/// `parent` is `None` for root commits.
pub fn semantic_delta(
    git: &Git,
    parent: Option<&GitOid>,
    commit: &GitOid,
    changed_paths: &[String],
) -> Result<SemanticDelta, SurfaceError> {
    let rs: Vec<&String> = changed_paths
        .iter()
        .filter(|p| p.ends_with(".rs"))
        .collect();
    let mut delta = SemanticDelta::default();
    if rs.is_empty() {
        return Ok(delta);
    }
    let after_map = crate_map_at(git, commit)?;
    let before_map = match parent {
        Some(p) => crate_map_at(git, p)?,
        None => CrateMap::default(),
    };
    let mut before = Vec::new();
    let mut after = Vec::new();
    for path in rs {
        let old = parent.and_then(|p| file_at(git, p, path));
        let new = file_at(git, commit, path);
        let b = symbols_of(&before_map, path, old, "before", &mut delta.unparsed);
        let a = symbols_of(&after_map, path, new, "after", &mut delta.unparsed);
        if b.is_some() || a.is_some() {
            delta.analyzed_files.push(path.clone());
        }
        before.extend(tagged(path, b.as_ref()));
        after.extend(tagged(path, a.as_ref()));
    }
    delta.changes = diff_symbols(&before, &after);
    Ok(delta)
}

/// Structural Cargo delta of `parent → commit` over the changed manifests.
pub fn dependency_delta(
    git: &Git,
    parent: Option<&GitOid>,
    commit: &GitOid,
    changed_paths: &[String],
) -> DependencyDelta {
    let mut delta = DependencyDelta::default();
    for path in changed_paths
        .iter()
        .filter(|p| p.rsplit('/').next() == Some(MANIFEST))
    {
        let old = parent.and_then(|p| file_at(git, p, path));
        let new = file_at(git, commit, path);
        match diff_manifest(path, old.as_deref(), new.as_deref()) {
            Ok(cs) => delta.changes.extend(cs),
            Err(reason) => delta.unparsed.push((path.clone(), reason)),
        }
    }
    delta
}

// ---------------------------------------------------------------------------
// Session side
// ---------------------------------------------------------------------------

/// A reference made by the session's work, with its file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReference {
    pub file: String,
    pub reference: Reference,
}

/// The symbol-level footprint of a session's speculative work (§11, derived
/// rather than declared): what it changed relative to its base, and what
/// its changed code relies on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSemantics {
    /// Repo-relative files changed in the workspace versus the base commit
    /// (committed or not, including new untracked files).
    pub changed_files: Vec<String>,
    /// Symbols the session added, removed, or modified.
    pub modified: Vec<SymbolChange>,
    /// References made by the session's changed Rust files.
    pub references: Vec<SessionReference>,
    /// External path roots the changed files use (candidate crate deps).
    pub external_crates: BTreeSet<String>,
    /// Manifest directories of the packages the session touched.
    pub crate_dirs: BTreeSet<String>,
    /// Files whose working copy did not parse and were analyzed from the
    /// workspace `HEAD` instead (the actor is mid-edit).
    pub fallback_files: Vec<String>,
    /// Files that could not be analyzed at all.
    pub unparsed: Vec<UnparsedFile>,
}

/// How a session's references relate to one changed symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Relation {
    /// The session names the symbol itself (call, type use, method/field
    /// access by name).
    Direct,
    /// The session names the symbol's owner (the struct of a field, the
    /// enum of a variant, the trait of a trait item, the type of a method).
    Owner,
    /// The session constructs the owning struct with a literal (no `..rest`).
    StructLiteral,
    /// The session matches exhaustively on the owning enum.
    ExhaustiveMatch,
    /// The session implements the owning trait.
    TraitImpl,
}

impl SessionSemantics {
    /// Whether the session itself changed `change`'s symbol.
    pub fn modifies(&self, change: &SymbolChange) -> bool {
        self.modified
            .iter()
            .any(|m| m.path == change.path && m.kind == change.kind)
    }

    /// Every session reference related to `change`, with the relation.
    pub fn relations<'s>(&'s self, change: &SymbolChange) -> Vec<(Relation, &'s SessionReference)> {
        let path = &change.path;
        let owner = if change.kind.is_member() && path.len() >= 2 {
            Some(&path[..path.len() - 1])
        } else {
            None
        };
        let name = path.last().map(String::as_str).unwrap_or_default();
        let mut out = Vec::new();
        for r in &self.references {
            let rel = match &r.reference.target {
                RefTarget::Path(p) => {
                    if change.kind != SymbolKind::Field && suffix_matches(path, p) {
                        Some(Relation::Direct)
                    } else if owner.is_some_and(|o| suffix_matches(o, p) || prefix_names(o, p)) {
                        Some(Relation::Owner)
                    } else if !change.kind.is_member() && prefix_names(path, p) {
                        // `Outcome::Retry` / `Router::new` use the type.
                        Some(Relation::Direct)
                    } else {
                        None
                    }
                }
                RefTarget::Method(m) => {
                    (matches!(change.kind, SymbolKind::Method | SymbolKind::TraitItem) && m == name)
                        .then_some(Relation::Direct)
                }
                RefTarget::Field(f) => {
                    (change.kind == SymbolKind::Field && f == name).then_some(Relation::Direct)
                }
                RefTarget::StructLiteral { path: p, has_rest } => {
                    if owner.is_some_and(|o| suffix_matches(o, p)) {
                        Some(if *has_rest {
                            Relation::Owner
                        } else {
                            Relation::StructLiteral
                        })
                    } else if suffix_matches(path, p) {
                        Some(Relation::Direct)
                    } else {
                        None
                    }
                }
                RefTarget::ExhaustiveMatch { enum_path } => {
                    if owner.is_some_and(|o| suffix_matches(o, enum_path)) {
                        Some(Relation::ExhaustiveMatch)
                    } else if suffix_matches(path, enum_path) {
                        Some(Relation::Direct)
                    } else {
                        None
                    }
                }
                RefTarget::TraitImpl { trait_path } => {
                    if owner.is_some_and(|o| suffix_matches(o, trait_path)) {
                        Some(Relation::TraitImpl)
                    } else if suffix_matches(path, trait_path) {
                        Some(Relation::Direct)
                    } else {
                        None
                    }
                }
            };
            if let Some(rel) = rel {
                out.push((rel, r));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.file.cmp(&b.1.file)));
        out
    }

    /// Whether the session touched the package whose manifest is at `dir`.
    pub fn touches_crate_dir(&self, dir: &str) -> bool {
        self.crate_dirs.contains(dir)
    }
}

/// `path` ends with the reference's segments (`[app, router, dispatch]`
/// ends with `[router, dispatch]` and `[dispatch]`).
fn suffix_matches(path: &[String], r: &[String]) -> bool {
    !r.is_empty() && r.len() <= path.len() && path[path.len() - r.len()..] == *r
}

/// The reference names `path` as a prefix (`Outcome::Retry` names
/// `Outcome`), matched by suffix on that prefix.
fn prefix_names(path: &[String], r: &[String]) -> bool {
    r.len() >= 2 && suffix_matches(path, &r[..r.len() - 1])
}

/// Analyze the workspace at `ws` against the session's `base` commit.
pub fn analyze_workspace(ws: &Git, base: &GitOid) -> Result<SessionSemantics, SurfaceError> {
    let mut sem = SessionSemantics::default();
    let mut changed: BTreeSet<String> = BTreeSet::new();
    for line in ws
        .run_git(&["diff", "--name-only", "--no-renames", base.as_str()])?
        .lines()
        .chain(
            ws.run_git(&["ls-files", "--others", "--exclude-standard"])?
                .lines(),
        )
    {
        if let Ok(p) = normalize_path(line) {
            changed.insert(p);
        }
    }
    sem.changed_files = changed.iter().cloned().collect();

    // Manifests as they stand in the workspace (tracked and new).
    let mut manifest_paths: BTreeSet<String> = ws
        .run_git(&["ls-files", "--cached", "--others", "--exclude-standard"])?
        .lines()
        .filter(|p| p.rsplit('/').next() == Some(MANIFEST))
        .filter_map(|p| normalize_path(p).ok())
        .collect();
    manifest_paths.retain(|p| read_regular(ws.repo_root(), p).is_some());
    let map_now = CrateMap::from_manifests(
        manifest_paths
            .iter()
            .filter_map(|p| read_regular(ws.repo_root(), p).map(|c| (p.clone(), c))),
    );
    let map_base = crate_map_at(ws, base)?;

    let head = ws.head_oid()?;
    let mut before = Vec::new();
    let mut after = Vec::new();
    for path in &changed {
        if let Some(root) = map_now.crate_of(path).or_else(|| map_base.crate_of(path)) {
            sem.crate_dirs.insert(root.dir.clone());
        }
        if path.rsplit('/').next() == Some(MANIFEST) {
            sem.crate_dirs.insert(manifest_dir(path));
        }
        if !path.ends_with(".rs") {
            continue;
        }
        let base_src = file_at(ws, base, path);
        let b = symbols_of(&map_base, path, base_src, "base", &mut Vec::new());
        // Working copy first; if the actor is mid-edit and it does not parse,
        // fall back to the workspace HEAD, then the base.
        let mut scratch = Vec::new();
        let current = read_regular(ws.repo_root(), path);
        let deleted = current.is_none();
        let mut a = if deleted {
            None
        } else {
            symbols_of(&map_now, path, current, "workspace", &mut scratch)
        };
        if a.is_none() && !deleted {
            let at_head = file_at(ws, &head, path);
            a = symbols_of(&map_now, path, at_head, "workspace HEAD", &mut Vec::new());
            // Analyzed from the workspace HEAD (a) or, failing that, the
            // base (b): either way the working copy itself was skipped.
            if a.is_some() || b.is_some() {
                sem.fallback_files.push(path.clone());
            } else {
                sem.unparsed.extend(scratch);
            }
        }
        before.extend(tagged(path, b.as_ref()));
        // Uses come from the newest version that parsed.
        let uses = a.as_ref().or(if deleted { None } else { b.as_ref() });
        if let Some(fs) = uses {
            sem.references
                .extend(fs.references.iter().map(|r| SessionReference {
                    file: path.clone(),
                    reference: r.clone(),
                }));
            sem.external_crates
                .extend(fs.external_roots.iter().cloned());
        }
        if a.is_some() || deleted {
            after.extend(tagged(path, a.as_ref()));
        } else {
            // Unanalyzable working copy: assume nothing changed rather than
            // reporting every symbol as removed.
            after.extend(tagged(path, b.as_ref()));
        }
    }
    sem.modified = diff_symbols(&before, &after);
    Ok(sem)
}

/// Read a workspace file only if it is a regular file inside the workspace
/// (§27: never follow a symlink out of the workspace).
fn read_regular(root: &Path, rel: &str) -> Option<String> {
    let full = root.join(rel);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    // A symlinked parent directory could still lead outside.
    let (canon, canon_root) = (full.canonicalize().ok()?, root.canonicalize().ok()?);
    if !canon.starts_with(&canon_root) {
        return None;
    }
    std::fs::read(&full)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}
