//! Symbol-level selective import (§15): bring chosen Rust items of a ghost
//! commit into a session's workspace, keeping everything else — including
//! the session's own versions of other items in the same file.
//!
//! For each file: `G'` = the ghost's parent `P` plus only the chosen units
//! of the ghost commit `G`; then `P → G'` is merged into the workspace
//! version `W` in converge mode. A chosen unit the session also changed is
//! contested and the import is refused — nothing is written. The result is
//! committed as a new workspace commit (never the ghost commit itself).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use doubledoor_core::GitOid;
use doubledoor_ghost::GhostDiff;
use doubledoor_git::Git;
use doubledoor_surface::semantic::crate_map_at;

use crate::error::ReconcileError;
use crate::merge::{merge_rust, merge_rust_selected, Mode};

/// Result of a selective import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    pub symbols: Vec<String>,
    pub files: Vec<String>,
    pub applied_commit: Option<GitOid>,
}

/// Import `symbols` (`rust:` ids from the ghost's semantic delta) into the
/// worktree at `ws`.
pub fn import_symbols(
    canonical: &Git,
    ws: &Path,
    ghost: &GhostDiff,
    symbols: &[String],
) -> Result<ImportOutcome, ReconcileError> {
    // Every chosen symbol must be one the ghost changed.
    let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for sym in symbols {
        let change = ghost
            .semantic
            .changes
            .iter()
            .find(|c| c.symbol.as_str() == sym)
            .ok_or_else(|| {
                ReconcileError::Usage(format!("symbol {sym} is not changed by ghost {}", ghost.id))
            })?;
        by_file
            .entry(change.file.clone())
            .or_default()
            .push(sym.clone());
    }
    let root = ghost.parent_commit.as_str().bytes().all(|b| b == b'0');
    if root {
        return Err(ReconcileError::Usage(
            "symbol import from a root commit is not supported; accept the file".to_string(),
        ));
    }
    let wsg = Git::new(ws.to_path_buf());
    let map = crate_map_at(canonical, &ghost.source_commit).unwrap_or_default();
    let mut writes: Vec<(String, String)> = Vec::new();
    for (file, syms) in &by_file {
        let dirty = wsg.run_git(&["status", "--porcelain", "--", file])?;
        if !dirty.is_empty() {
            return Err(ReconcileError::ImportConflict(format!(
                "workspace has uncommitted changes to {file}"
            )));
        }
        let module = map.module_of(file).ok_or_else(|| {
            ReconcileError::Usage(format!("{file} is not part of a Cargo package"))
        })?;
        let show = |c: &GitOid| -> Result<String, ReconcileError> {
            let bytes = canonical.run_git_bytes(&["show", &format!("{c}:{file}")])?;
            String::from_utf8(bytes)
                .map_err(|_| ReconcileError::Usage(format!("{file} is not UTF-8")))
        };
        let p = show(&ghost.parent_commit)?;
        let g = show(&ghost.source_commit)?;
        let w = fs::read_to_string(ws.join(file))
            .map_err(|e| ReconcileError::ImportConflict(format!("{file}: {e}")))?;
        // A symbol selects its own unit, or the unit owning it (a field or
        // variant selects its struct/enum).
        let chosen = |unit: &str| {
            syms.iter().any(|s| {
                s == unit
                    || s.strip_prefix(unit)
                        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with("::"))
            })
        };
        let g_sel = merge_rust_selected(&p, &p, &g, &module, Mode::Speculative, Some(&chosen))
            .map_err(|e| ReconcileError::ImportConflict(e.0))?
            .content
            .unwrap_or_default();
        let merged = merge_rust(&p, &w, &g_sel, &module, Mode::Converge)
            .map_err(|e| ReconcileError::ImportConflict(e.0))?;
        if !merged.contested.is_empty() {
            return Err(ReconcileError::ImportConflict(format!(
                "the session also changed {}; import it by reconciliation instead",
                merged
                    .contested
                    .iter()
                    .map(|c| c.surface.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        writes.push((file.clone(), merged.content.unwrap_or_default()));
    }
    let mut changed = Vec::new();
    for (file, text) in &writes {
        let current = fs::read_to_string(ws.join(file))?;
        if &current != text {
            fs::write(ws.join(file), text)?;
            changed.push(file.clone());
        }
    }
    if changed.is_empty() {
        return Ok(ImportOutcome {
            symbols: symbols.to_vec(),
            files: Vec::new(),
            applied_commit: None,
        });
    }
    let author = canonical.run_git(&[
        "show",
        "-s",
        "--format=%an <%ae>",
        ghost.source_commit.as_str(),
    ])?;
    let message = format!(
        "doubledoor: import {} symbol(s) from ghost {}\n\n{}\n\nDd-Ghost: {}\nDd-Source-Commit: {}\n",
        symbols.len(),
        ghost.id,
        symbols.join("\n"),
        ghost.id,
        ghost.source_commit
    );
    let mut args = vec![
        "commit",
        "-q",
        "--no-verify",
        "--author",
        author.as_str(),
        "-m",
        message.as_str(),
        "--",
    ];
    args.extend(changed.iter().map(String::as_str));
    if let Err(e) = wsg.run_git(&args) {
        let mut restore = vec!["checkout", "HEAD", "--"];
        restore.extend(changed.iter().map(String::as_str));
        let _ = wsg.run_git(&restore);
        return Err(e.into());
    }
    Ok(ImportOutcome {
        symbols: symbols.to_vec(),
        files: changed,
        applied_commit: Some(wsg.head_oid()?),
    })
}
