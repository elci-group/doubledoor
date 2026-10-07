//! The unified evidence model in practice (§17, §18, §21, §49 Phase V).
//!
//! [`collect`] gathers everything known about a reconciliation candidate —
//! Doubledoor's own source, API, dependency, compiler, and test evidence
//! ([`internal`]), plus whichever optional ELCI providers are installed
//! ([`providers`]: Kaptaind, Traci, Fract, ZOC, and any
//! `doubledoor.evidence/v1` provider such as Sore) — into one
//! [`EvidenceBundle`], computes the unified verdict from deterministic
//! evidence only ([`unify`]), records provider disagreements, separates the
//! actor's claim from evidence, and optionally publishes the graph to
//! Padagonia ([`padagonia`]).
//!
//! Providers run against private throwaway worktrees of the candidate —
//! never the session's workspace or the canonical worktree — because some
//! of them write state into the tree they analyse (Kaptaind creates
//! `.kaptaind/`). A missing, failing, or misbehaving provider is reported
//! and contributes nothing; Doubledoor's own evidence is always there.

#![forbid(unsafe_code)]

pub mod config;
pub mod internal;
pub mod padagonia;
pub mod providers;
pub mod unify;

use std::fs;
use std::path::Path;
use std::time::Instant;

use doubledoor_core::evidence::{EvidenceBundle, ProviderRun, ProviderStatus, EVIDENCE_SCHEMA};
use doubledoor_git::Git;
use doubledoor_reconcile::compat::CompatResult;
use doubledoor_reconcile::Candidate;
use doubledoor_validate::ValidationReport;

pub use config::EvidenceConfig;

/// Inputs to [`collect`].
pub struct Context<'a> {
    pub git: &'a Git,
    pub candidate: &'a Candidate,
    pub validation: Option<&'a ValidationReport>,
    pub compatibility: &'a [CompatResult],
    /// The validation run's log directory (full gate output).
    pub validation_logs: Option<&'a Path>,
    /// The session's stated intent (the actor's claim, §18).
    pub intent: &'a str,
    /// Scratch directory owned by the caller (worktrees + logs).
    pub work: &'a Path,
    pub config: &'a EvidenceConfig,
}

/// Gather, unify, and (optionally) publish the evidence for a candidate.
pub fn collect(ctx: &Context<'_>) -> EvidenceBundle {
    let c = ctx.candidate;
    let started = Instant::now();
    let mut items = internal::collect(
        ctx.git,
        c,
        ctx.validation,
        ctx.compatibility,
        ctx.validation_logs,
    );
    let mut runs = vec![ProviderRun {
        provider: "doubledoor".into(),
        tier: Some(doubledoor_core::evidence::EvidenceTier::GitObject),
        status: ProviderStatus::Ran { items: items.len() },
        duration_ms: started.elapsed().as_millis() as u64,
        command: Vec::new(),
    }];

    let logs = ctx.work.join("logs");
    let tree = ctx.work.join("tree");
    let staged = ctx.work.join("staged");
    let request = ctx.work.join("request.json");
    let _ = fs::create_dir_all(&logs);
    let materialized = materialize(ctx.git, c, &tree, &staged);
    let _ = fs::write(
        &request,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": EVIDENCE_SCHEMA,
            "candidate": c.id,
            "commit": c.commit,
            "target": c.target,
            "base": c.base,
            "files": c.promotes_files,
            "contested": c.contested,
            "intent": ctx.intent,
            "diagnostics": ctx.validation.map(|v| &v.diagnostics),
        }))
        .unwrap_or_default(),
    );

    match materialized {
        Ok(()) => {
            let (commit, target, base) = (
                c.commit.to_string(),
                c.target.to_string(),
                c.base.to_string(),
            );
            let (tree_s, req_s) = (
                tree.to_string_lossy().into_owned(),
                request.to_string_lossy().into_owned(),
            );
            let env = [
                ("DOUBLEDOOR_CANDIDATE", commit.as_str()),
                ("DOUBLEDOOR_TARGET", target.as_str()),
                ("DOUBLEDOOR_BASE", base.as_str()),
                ("DOUBLEDOOR_CANDIDATE_TREE", tree_s.as_str()),
                ("DOUBLEDOOR_REQUEST", req_s.as_str()),
            ];
            let t = providers::Target {
                tree: &tree,
                staged: &staged,
                files: &c.promotes_files,
                intent: ctx.intent,
                logs: &logs,
                request: &request,
                env: &env,
            };
            let cfg = ctx.config;
            let mut outcomes = vec![
                providers::kaptaind(&cfg.kaptaind, &t),
                providers::traci(&cfg.traci, &t),
                providers::fract(&cfg.fract, &t),
                providers::zoc(&cfg.zoc, &t),
            ];
            if !cfg.external.contains_key("sore") {
                outcomes.push((Vec::new(), ProviderRun {
                    provider: "sore".into(),
                    tier: Some(doubledoor_core::evidence::EvidenceTier::Model),
                    status: ProviderStatus::Unavailable {
                        reason: if config::resolve_bin("sore", None).is_some() {
                            "installed but not configured: add it under `external.sore` in evidence.json (doubledoor.evidence/v1)".into()
                        } else {
                            "`sore` is not installed".into()
                        },
                    },
                    duration_ms: 0,
                    command: Vec::new(),
                }));
            }
            for p in cfg.external.values() {
                outcomes.push(providers::external(p, &t));
            }
            for (its, run) in outcomes {
                items.extend(its);
                runs.push(run);
            }
        }
        Err(reason) => {
            for name in ["kaptaind", "traci", "fract", "zoc"] {
                runs.push(ProviderRun {
                    provider: name.into(),
                    tier: None,
                    status: ProviderStatus::Failed {
                        reason: format!("candidate could not be materialized: {reason}"),
                    },
                    duration_ms: 0,
                    command: Vec::new(),
                });
            }
        }
    }
    dematerialize(ctx.git, &tree, &staged);

    let conflicts = unify::conflicts(&items);
    let explanation = unify::explanation(ctx.intent, &items);
    let assessment = unify::assess(&items);
    let mut bundle = EvidenceBundle {
        schema: EVIDENCE_SCHEMA.to_string(),
        candidate: c.id.to_string(),
        commit: c.commit.to_string(),
        items,
        providers: runs,
        conflicts,
        explanation,
        assessment,
    };
    let edges: Vec<(String, String)> = c
        .graph
        .edges
        .iter()
        .map(|e| (e.from.clone(), e.to.clone()))
        .collect();
    let pub_run = padagonia::publish(&ctx.config.padagonia, &bundle, &edges);
    bundle.providers.push(pub_run);
    bundle
}

/// `tree`: the candidate checked out. `staged`: the target checked out with
/// the candidate's content staged over it (what tools that analyse
/// "uncommitted changes" need).
fn materialize(git: &Git, c: &Candidate, tree: &Path, staged: &Path) -> Result<(), String> {
    dematerialize(git, tree, staged);
    let s = |p: &Path| p.to_string_lossy().into_owned();
    git.run_git(&[
        "worktree",
        "add",
        "--detach",
        "-q",
        &s(tree),
        c.commit.as_str(),
    ])
    .map_err(|e| e.to_string())?;
    git.run_git(&[
        "worktree",
        "add",
        "--detach",
        "-q",
        &s(staged),
        c.target.as_str(),
    ])
    .map_err(|e| e.to_string())?;
    Git::new(staged.to_path_buf())
        .run_git(&["read-tree", "-u", "--reset", c.commit.as_str()])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn dematerialize(git: &Git, tree: &Path, staged: &Path) {
    for p in [tree, staged] {
        if let Some(s) = p.to_str() {
            let _ = git.run_git(&["worktree", "remove", "--force", s]);
        }
        let _ = fs::remove_dir_all(p);
    }
    let _ = git.run_git(&["worktree", "prune"]);
}
