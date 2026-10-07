//! Candidate construction (§16): freeze → snapshot → resolve ghosts →
//! conflict graph → independent surfaces → deterministic imports →
//! structural conflicts → construct candidate.
//!
//! A candidate is an ordinary commit whose parent is the canonical `HEAD`
//! it was built against (its *target*), so promotion is a fast-forward that
//! compare-and-swaps that exact `HEAD` (§23, DD-I4). It is built entirely
//! in private: the session's working copy is snapshotted through a
//! temporary index, and the candidate tree is written through another —
//! neither the session's workspace, the canonical worktree, nor any ref is
//! touched (DD-I1, DD-I2). Nothing here is irreversible: an unused
//! candidate is an unreferenced commit.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use doubledoor_core::{
    CandidateId, DoorSession, DoorSessionId, GhostDecision, GhostDiffId, GitOid,
};
use doubledoor_ghost::GhostDiff;
use doubledoor_git::Git;
use doubledoor_surface::cratemap::CrateMap;
use doubledoor_surface::semantic::{crate_map_at, semantic_delta, SessionSemantics};

use crate::error::ReconcileError;
use crate::merge::{merge_rust, Contested, Method, Mode, Side};

/// How one path ended up in the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileResolution {
    pub path: String,
    pub method: Method,
    /// `session`, `canonical`, `merged`, or `deleted`.
    pub outcome: String,
    pub contested: Vec<Contested>,
    /// Merged files: the units the session contributed without contest.
    #[serde(default)]
    pub from_session: Vec<String>,
}

/// Whether the actor has answered for a contested surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    /// Every ghost touching the surface was accepted or rejected.
    Resolved,
    /// A ghost touching the surface is deferred or undecided.
    Unresolved { ghosts: Vec<GhostDiffId> },
}

/// A contested surface of the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContestedSurface {
    pub surface: String,
    pub file: String,
    pub kept: Side,
    pub resolution: Resolution,
}

/// Where a surface's change came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Origin {
    Session,
    Canonical,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    pub surface: String,
    pub origin: Origin,
    /// `imported` (one-sided, taken), `contested`, or `merged`.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// A session surface (`rust:<referencing item>`).
    pub from: String,
    /// A canonical-changed surface it relies on.
    pub to: String,
    /// `depends_on` (the session's code references it).
    pub kind: String,
    /// Canonical's change is API-breaking.
    pub breaking: bool,
}

/// The conflict graph (§16 step 4): which surfaces each side changed, which
/// collide, and which session code depends on canonical changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Lifecycle of a candidate record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateStatus {
    Built,
    Validated {
        passed: bool,
    },
    /// Construction or validation did not finish (§43: never presented as
    /// complete).
    Incomplete {
        reason: String,
    },
    /// Canonical advanced past the target.
    Stale,
    Discarded,
    Promoted {
        partial: bool,
    },
}

/// A reconciliation candidate (§16, §52 `E'`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: CandidateId,
    pub session: DoorSessionId,
    pub mode: Mode,
    pub base: GitOid,
    /// Canonical `HEAD` the candidate was built on (its parent).
    pub target: GitOid,
    /// Commit capturing the session's workspace (incl. uncommitted work).
    pub snapshot: GitOid,
    pub snapshot_tree: GitOid,
    pub commit: GitOid,
    pub tree: GitOid,
    pub files: Vec<FileResolution>,
    pub contested: Vec<ContestedSurface>,
    pub graph: ConflictGraph,
    /// Files the candidate changes relative to its target (what promotion
    /// would mutate, and what the Locksmith lease must cover).
    pub promotes_files: Vec<String>,
    pub promotable: bool,
    /// Why not promotable.
    pub blocking: Vec<String>,
    pub status: CandidateStatus,
    pub created_at_millis: u64,
}

impl Candidate {
    /// Contested surfaces exist and stay isolated in the session (a
    /// converge-mode candidate promotes only the uncontested part).
    pub fn is_partial(&self) -> bool {
        self.mode == Mode::Converge && !self.contested.is_empty()
    }
}

/// Snapshot the working copy at `ws` (tracked, modified, and untracked
/// non-ignored files) as a commit whose parent is the workspace `HEAD`,
/// through a private index in `scratch`. The workspace is not modified.
/// Returns `(commit, tree)`; the commit is `HEAD` itself when clean.
pub fn snapshot_workspace(
    ws: &Git,
    scratch: &Path,
    actor: &str,
) -> Result<(GitOid, GitOid), ReconcileError> {
    fs::create_dir_all(scratch)?;
    let index = scratch.join("snapshot.index");
    let _ = fs::remove_file(&index);
    let index_s = path_str(&index)?;
    let env = [("GIT_INDEX_FILE", index_s)];
    ws.run_git_env(&env, &["read-tree", "HEAD"])?;
    ws.run_git_env(&env, &["add", "-A"])?;
    let tree = oid(&ws.run_git_env(&env, &["write-tree"])?)?;
    let _ = fs::remove_file(&index);
    let head = ws.head_oid()?;
    let head_tree = oid(&ws.run_git(&["rev-parse", &format!("{head}^{{tree}}")])?)?;
    if tree == head_tree {
        return Ok((head, tree));
    }
    let commit = commit_tree(
        ws,
        &tree,
        &head,
        &format!("doubledoor: snapshot of {actor}'s working copy"),
        actor,
    )?;
    Ok((commit, tree))
}

/// Inputs to [`build`].
pub struct BuildInput<'a> {
    pub canonical: &'a Git,
    pub workspace: &'a Git,
    pub session: &'a DoorSession,
    pub ghosts: &'a [GhostDiff],
    pub decisions: &'a HashMap<GhostDiffId, GhostDecision>,
    pub semantics: Option<&'a SessionSemantics>,
    pub mode: Mode,
    /// Scratch directory for private indexes and merge inputs.
    pub scratch: &'a Path,
}

/// Construct a candidate against the canonical `HEAD`.
pub fn build(input: &BuildInput<'_>) -> Result<Candidate, ReconcileError> {
    let git = input.canonical;
    let session = input.session;
    let base = session.base.commit.clone();
    let actor = session.actor.as_str();
    let (snapshot, snapshot_tree) = snapshot_workspace(input.workspace, input.scratch, actor)?;
    // Derived from known state: the snapshot descends from the base.
    if git
        .run_git_status(&[
            "merge-base",
            "--is-ancestor",
            base.as_str(),
            snapshot.as_str(),
        ])?
        .0
        != 0
    {
        return Err(ReconcileError::UnknownState(format!(
            "workspace history {snapshot} does not descend from the session base {base}"
        )));
    }
    let target = git.head_oid()?;

    let changed = |from: &GitOid, to: &GitOid| -> Result<BTreeSet<String>, ReconcileError> {
        Ok(git
            .run_git(&[
                "diff",
                "--name-only",
                "--no-renames",
                from.as_str(),
                to.as_str(),
            ])?
            .lines()
            .filter(|l| !l.is_empty())
            .map(ToString::to_string)
            .collect())
    };
    let by_session = changed(&base, &snapshot)?;
    let by_canonical = changed(&base, &target)?;
    let map = crate_map_at(git, &target).unwrap_or_default();

    let mut files = Vec::new();
    let mut updates: Vec<(String, Option<Blob>)> = Vec::new();
    for path in by_session.iter() {
        let b = blob(git, &base, path)?;
        let h = blob(git, &target, path)?;
        let s = blob(git, &snapshot, path)?;
        if !by_canonical.contains(path) || s == h {
            if s != h {
                updates.push((path.clone(), s.clone()));
            }
            files.push(FileResolution {
                path: path.clone(),
                method: Method::OneSided,
                outcome: if s.is_none() { "deleted" } else { "session" }.to_string(),
                contested: Vec::new(),
                from_session: Vec::new(),
            });
            continue;
        }
        let res = merge_file(git, &map, path, b, h, s, input.mode, input.scratch)?;
        updates.push((path.clone(), res.1));
        files.push(res.0);
    }
    for path in by_canonical.difference(&by_session) {
        files.push(FileResolution {
            path: path.clone(),
            method: Method::OneSided,
            outcome: "canonical".to_string(),
            contested: Vec::new(),
            from_session: Vec::new(),
        });
    }

    let tree = write_tree(git, &target, &updates, input.scratch)?;
    let id = CandidateId::new();
    let commit = commit_tree(
        git,
        &tree,
        &target,
        &format!(
            "{}\n\nReconciled by Doubledoor ({:?} mode).\n\nDd-Session: {}\nDd-Candidate: {}\nDd-Actor: {}\nDd-Base: {}\nDd-Target: {}\n",
            session.intent.summary, input.mode, session.id, id, actor, base, target
        ),
        actor,
    )?;
    let promotes_files: Vec<String> = git
        .run_git(&[
            "diff",
            "--name-only",
            "--no-renames",
            target.as_str(),
            commit.as_str(),
        ])?
        .lines()
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect();

    let contested = resolve_contested(&files, input.ghosts, input.decisions);
    let graph = conflict_graph(git, &base, &target, &files, &by_canonical, input.semantics);

    let mut blocking = Vec::new();
    for g in input.ghosts {
        if g.classification.needs_assessment() && !input.decisions.contains_key(&g.id) {
            blocking.push(format!("ghost {} is not yet assessed", g.id));
        }
    }
    if input.mode == Mode::Speculative {
        for c in &contested {
            if let Resolution::Unresolved { ghosts } = &c.resolution {
                blocking.push(format!(
                    "contested surface {} keeps the session's version but ghost(s) {} touching it are deferred or undecided",
                    c.surface,
                    ghosts.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    Ok(Candidate {
        id,
        session: session.id,
        mode: input.mode,
        base,
        target,
        snapshot,
        snapshot_tree,
        commit,
        tree,
        files,
        contested,
        graph,
        promotes_files,
        promotable: blocking.is_empty(),
        blocking,
        status: CandidateStatus::Built,
        created_at_millis: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    })
}

/// A blob in a commit: object id plus file mode.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Blob {
    oid: String,
    mode: String,
}

fn blob(git: &Git, commit: &GitOid, path: &str) -> Result<Option<Blob>, ReconcileError> {
    let out = git.run_git(&["ls-tree", commit.as_str(), "--", path])?;
    // `<mode> <type> <oid>\t<path>`
    Ok(out.lines().next().and_then(|l| {
        let (meta, _) = l.split_once('\t')?;
        let mut f = meta.split(' ');
        let mode = f.next()?.to_string();
        let kind = f.next()?;
        let oid = f.next()?.to_string();
        (kind == "blob").then_some(Blob { oid, mode })
    }))
}

fn content(git: &Git, b: &Blob) -> Result<Vec<u8>, ReconcileError> {
    Ok(git.run_git_bytes(&["cat-file", "blob", &b.oid])?)
}

/// Merge one path changed by both sides. Returns the resolution and the
/// blob to write (`None` = delete).
#[allow(clippy::too_many_arguments)]
fn merge_file(
    git: &Git,
    map: &CrateMap,
    path: &str,
    b: Option<Blob>,
    h: Option<Blob>,
    s: Option<Blob>,
    mode: Mode,
    scratch: &Path,
) -> Result<(FileResolution, Option<Blob>), ReconcileError> {
    let whole = |kept: Side| -> (FileResolution, Option<Blob>) {
        let (blob, outcome) = match kept {
            Side::Session => (s.clone(), if s.is_none() { "deleted" } else { "session" }),
            Side::Canonical => (h.clone(), if h.is_none() { "deleted" } else { "canonical" }),
        };
        (
            FileResolution {
                path: path.to_string(),
                method: Method::Textual,
                outcome: outcome.to_string(),
                contested: vec![Contested {
                    key: path.to_string(),
                    surface: format!("file:{path}"),
                    kept,
                }],
                from_session: Vec::new(),
            },
            blob,
        )
    };
    let kept = match mode {
        Mode::Speculative => Side::Session,
        Mode::Converge => Side::Canonical,
    };
    // Delete vs. modify, or add vs. add: the whole file is contested.
    let (Some(bb), Some(hb), Some(sb)) = (&b, &h, &s) else {
        return Ok(whole(kept));
    };
    let (bt, ht, st) = (content(git, bb)?, content(git, hb)?, content(git, sb)?);
    let file_mode = sb.mode.clone();

    if let (Some(module), Ok(bs), Ok(hs), Ok(ss)) = (
        path.ends_with(".rs").then(|| map.module_of(path)).flatten(),
        std::str::from_utf8(&bt),
        std::str::from_utf8(&ht),
        std::str::from_utf8(&st),
    ) {
        if let Ok(m) = merge_rust(bs, hs, ss, &module, mode) {
            let text = m.content.unwrap_or_default();
            let oid = hash(git, scratch, text.as_bytes())?;
            return Ok((
                FileResolution {
                    path: path.to_string(),
                    method: Method::Units,
                    outcome: "merged".to_string(),
                    contested: m.contested,
                    from_session: m.from_session,
                },
                Some(Blob {
                    oid,
                    mode: file_mode,
                }),
            ));
        }
    }

    // Textual three-way merge.
    fs::create_dir_all(scratch)?;
    let (pb, ph, ps) = (
        scratch.join("merge.base"),
        scratch.join("merge.ours"),
        scratch.join("merge.theirs"),
    );
    fs::write(&pb, &bt)?;
    fs::write(&ph, &ht)?;
    fs::write(&ps, &st)?;
    let (code, merged, _) = git.run_git_status(&[
        "merge-file",
        "-p",
        path_str(&ph)?,
        path_str(&pb)?,
        path_str(&ps)?,
    ])?;
    if code == 0 {
        let oid = hash(git, scratch, &merged)?;
        return Ok((
            FileResolution {
                path: path.to_string(),
                method: Method::Textual,
                outcome: "merged".to_string(),
                contested: Vec::new(),
                from_session: Vec::new(),
            },
            Some(Blob {
                oid,
                mode: file_mode,
            }),
        ));
    }
    Ok(whole(kept))
}

fn hash(git: &Git, scratch: &Path, bytes: &[u8]) -> Result<String, ReconcileError> {
    let p = scratch.join("blob.tmp");
    fs::write(&p, bytes)?;
    let oid = git.run_git(&["hash-object", "-w", "--no-filters", path_str(&p)?])?;
    let _ = fs::remove_file(&p);
    Ok(oid)
}

/// `target`'s tree with `updates` applied, via a private index.
fn write_tree(
    git: &Git,
    target: &GitOid,
    updates: &[(String, Option<Blob>)],
    scratch: &Path,
) -> Result<GitOid, ReconcileError> {
    fs::create_dir_all(scratch)?;
    let index = scratch.join("candidate.index");
    let _ = fs::remove_file(&index);
    let env = [("GIT_INDEX_FILE", path_str(&index)?)];
    git.run_git_env(&env, &["read-tree", target.as_str()])?;
    for (path, blob) in updates {
        match blob {
            Some(b) => {
                let info = format!("{},{},{}", b.mode, b.oid, path);
                git.run_git_env(&env, &["update-index", "--add", "--cacheinfo", &info])?;
            }
            None => {
                git.run_git_env(&env, &["update-index", "--force-remove", "--", path])?;
            }
        }
    }
    let tree = oid(&git.run_git_env(&env, &["write-tree"])?)?;
    let _ = fs::remove_file(&index);
    Ok(tree)
}

/// `git commit-tree`, authored as the session actor. The committer is the
/// repository's configured identity when there is one.
fn commit_tree(
    git: &Git,
    tree: &GitOid,
    parent: &GitOid,
    message: &str,
    actor: &str,
) -> Result<GitOid, ReconcileError> {
    let email = format!(
        "{}@doubledoor.invalid",
        actor.replace(char::is_whitespace, "-")
    );
    let mut env: Vec<(&str, &str)> = vec![("GIT_AUTHOR_NAME", actor), ("GIT_AUTHOR_EMAIL", &email)];
    let configured = git.run_git(&["config", "user.email"]).is_ok();
    if !configured {
        env.push(("GIT_COMMITTER_NAME", "doubledoor"));
        env.push(("GIT_COMMITTER_EMAIL", "doubledoor@doubledoor.invalid"));
    }
    let out = git.run_git_env(
        &env,
        &[
            "commit-tree",
            tree.as_str(),
            "-p",
            parent.as_str(),
            "-m",
            message,
        ],
    )?;
    oid(&out)
}

/// Contested surfaces and whether the actor has answered for them: a
/// contested file's surfaces are resolved when every ghost that touched the
/// file has a final decision (accept or reject).
fn resolve_contested(
    files: &[FileResolution],
    ghosts: &[GhostDiff],
    decisions: &HashMap<GhostDiffId, GhostDecision>,
) -> Vec<ContestedSurface> {
    let mut out = Vec::new();
    for f in files {
        let file_surface = format!("file:{}", f.path);
        let touching: Vec<&GhostDiff> = ghosts
            .iter()
            .filter(|g| {
                g.affected_surfaces
                    .iter()
                    .any(|s| s.as_str() == file_surface)
            })
            .collect();
        for c in &f.contested {
            let open: Vec<GhostDiffId> = touching
                .iter()
                .filter(|g| !decisions.get(&g.id).is_some_and(GhostDecision::is_final))
                .map(|g| g.id)
                .collect();
            out.push(ContestedSurface {
                surface: c.surface.clone(),
                file: f.path.clone(),
                kept: c.kept,
                resolution: if open.is_empty() {
                    Resolution::Resolved
                } else {
                    Resolution::Unresolved { ghosts: open }
                },
            });
        }
    }
    out
}

fn conflict_graph(
    git: &Git,
    base: &GitOid,
    target: &GitOid,
    files: &[FileResolution],
    by_canonical: &BTreeSet<String>,
    semantics: Option<&SessionSemantics>,
) -> ConflictGraph {
    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();
    for f in files {
        let (origin, status) = match (f.outcome.as_str(), f.contested.is_empty(), f.method) {
            ("canonical", true, Method::OneSided) => (Origin::Canonical, "imported"),
            (_, true, Method::OneSided) => (Origin::Session, "imported"),
            (_, true, _) => (Origin::Both, "merged"),
            _ => (Origin::Both, "contested"),
        };
        nodes.insert(
            format!("file:{}", f.path),
            GraphNode {
                surface: format!("file:{}", f.path),
                origin,
                status: status.to_string(),
            },
        );
        for c in &f.contested {
            nodes.insert(
                c.surface.clone(),
                GraphNode {
                    surface: c.surface.clone(),
                    origin: Origin::Both,
                    status: "contested".to_string(),
                },
            );
        }
    }
    let mut edges = Vec::new();
    if let Some(sem) = semantics {
        let paths: Vec<String> = by_canonical.iter().cloned().collect();
        if let Ok(delta) = semantic_delta(git, Some(base), target, &paths) {
            for change in &delta.changes {
                let breaking = change.impact == doubledoor_surface::semantic::ApiImpact::Breaking;
                let mut froms = BTreeSet::new();
                for (_, r) in sem.relations(change) {
                    froms.insert(format!("rust:{}", r.reference.from));
                }
                if !froms.is_empty() {
                    nodes
                        .entry(change.symbol.as_str().to_string())
                        .or_insert(GraphNode {
                            surface: change.symbol.as_str().to_string(),
                            origin: Origin::Canonical,
                            status: "imported".to_string(),
                        });
                }
                for from in froms {
                    edges.push(GraphEdge {
                        from,
                        to: change.symbol.as_str().to_string(),
                        kind: "depends_on".to_string(),
                        breaking,
                    });
                }
            }
        }
    }
    ConflictGraph {
        nodes: nodes.into_values().collect(),
        edges,
    }
}

fn oid(s: &str) -> Result<GitOid, ReconcileError> {
    GitOid::parse(s.trim())
        .map_err(|_| ReconcileError::UnknownState(format!("not an object id: {s}")))
}

fn path_str(p: &Path) -> Result<&str, ReconcileError> {
    p.to_str()
        .ok_or_else(|| ReconcileError::UnknownState(format!("path is not UTF-8: {}", p.display())))
}

/// Where candidates are stored: `<session dir>/candidates/<id>.json`.
pub fn candidate_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("candidates")
}
