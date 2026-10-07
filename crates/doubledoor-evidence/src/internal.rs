//! Doubledoor's own evidence about a candidate — always available, all
//! deterministic (§17 levels 1–6):
//!
//! | provider | kind | tier |
//! |---|---|---|
//! | `doubledoor.merge` | source: derivation, per-file merge method, contests | git object / AST |
//! | `doubledoor.ast` | API: public signatures the promotion changes | AST |
//! | `doubledoor.cargo` | dependency: manifest changes the promotion carries | dependency |
//! | `doubledoor.graph` | dependency: retained code relying on canonical changes | AST, or compiler once checked |
//! | `doubledoor.compiler` | diagnostic: `cargo check` errors, per-item compatibility | compiler |
//! | `doubledoor.gates` | diagnostic: configured repository checks | static analysis |
//! | `doubledoor.tests` | behavioural: `cargo test` results | test |

use std::path::Path;

use doubledoor_core::evidence::{EvidenceItem, EvidenceKind as K, EvidenceTier as T, Verdict as V};
use doubledoor_git::Git;
use doubledoor_reconcile::candidate::Resolution;
use doubledoor_reconcile::compat::CompatResult;
use doubledoor_reconcile::merge::{Method, Side};
use doubledoor_reconcile::Candidate;
use doubledoor_surface::cargo::CargoChangeKind;
use doubledoor_surface::rust::SymbolVisibility;
use doubledoor_surface::semantic::{dependency_delta, semantic_delta, ApiImpact};
use doubledoor_validate::{GateStatus, ValidationReport};

/// Built-in gate names (anything else is a configured repository check).
const BUILTIN_GATES: [&str; 4] = [
    "git integrity",
    "cargo metadata",
    "cargo check",
    "cargo test",
];

pub fn collect(
    git: &Git,
    c: &Candidate,
    validation: Option<&ValidationReport>,
    compat: &[CompatResult],
    logs: Option<&Path>,
) -> Vec<EvidenceItem> {
    let mut out = Vec::new();
    source(git, c, validation, &mut out);
    api(git, c, &mut out);
    dependency(git, c, compat, &mut out);
    if let Some(v) = validation {
        diagnostics(v, compat, &mut out);
        behaviour(v, logs, &mut out);
    }
    out
}

fn source(git: &Git, c: &Candidate, v: Option<&ValidationReport>, out: &mut Vec<EvidenceItem>) {
    let p = "doubledoor.merge";
    match doubledoor_reconcile::promote::verify_derivation(git, c) {
        Ok(()) => out.push(EvidenceItem::new(
            p,
            K::Source,
            T::GitObject,
            V::Supports,
            None,
            format!(
                "candidate {} is derived from canonical {} and the session snapshot {}",
                short(c.commit.as_str()),
                short(c.target.as_str()),
                short(c.snapshot.as_str())
            ),
        )),
        Err(e) => out.push(EvidenceItem::new(
            p,
            K::Source,
            T::GitObject,
            V::Blocks,
            None,
            e.to_string(),
        )),
    }
    if let Some(g) = v.and_then(|v| v.gates.iter().find(|g| g.name == "git integrity")) {
        let verdict = if g.status.passed() {
            V::Supports
        } else {
            V::Blocks
        };
        out.push(
            EvidenceItem::new(
                p,
                K::Source,
                T::GitObject,
                verdict,
                None,
                "every object the candidate adds is present",
            )
            .by(&g.argv),
        );
    }
    for f in &c.files {
        let tier = if f.method == Method::Units {
            T::Ast
        } else {
            T::GitObject
        };
        out.push(
            EvidenceItem::new(
                p,
                K::Source,
                tier,
                V::Info,
                Some(format!("file:{}", f.path)),
                format!("{}: {} ({:?})", f.path, f.outcome, f.method).to_lowercase(),
            )
            .at(f.path.clone(), None),
        );
    }
    for x in &c.contested {
        let (verdict, text) = match (&x.resolution, x.kept) {
            (Resolution::Unresolved { .. }, Side::Session) => (
                V::Concern,
                format!("contested {} keeps the session's version, but the ghost touching it is deferred or undecided", x.surface),
            ),
            (_, Side::Canonical) => (
                V::Info,
                format!("contested {} keeps canonical's version; the session's version stays isolated", x.surface),
            ),
            (Resolution::Resolved, Side::Session) => (
                V::Info,
                format!("contested {} keeps the session's version by the actor's explicit decision", x.surface),
            ),
        };
        out.push(
            EvidenceItem::new(p, K::Source, T::Ast, verdict, Some(x.surface.clone()), text)
                .at(x.file.clone(), None),
        );
    }
}

fn api(git: &Git, c: &Candidate, out: &mut Vec<EvidenceItem>) {
    let p = "doubledoor.ast";
    match semantic_delta(git, Some(&c.target), &c.commit, &c.promotes_files) {
        Ok(d) => {
            let mut breaking = 0;
            for ch in &d.changes {
                if ch.visibility != SymbolVisibility::Public {
                    continue;
                }
                let (verdict, what) = match ch.impact {
                    ApiImpact::Breaking => {
                        breaking += 1;
                        (V::Concern, "breaking public API change")
                    }
                    ApiImpact::Additive => (V::Info, "additive public API change"),
                    ApiImpact::Behavioral => continue,
                };
                out.push(
                    EvidenceItem::new(
                        p,
                        K::Api,
                        T::Ast,
                        verdict,
                        Some(ch.symbol.as_str().to_string()),
                        format!("{what}: `{}` {}", ch.qualified(), ch.describe()),
                    )
                    .at(ch.file.clone(), Some(ch.line)),
                );
            }
            for u in &d.unparsed {
                out.push(EvidenceItem::new(
                    p,
                    K::Api,
                    T::Ast,
                    V::Concern,
                    Some(format!("file:{}", u.path)),
                    format!("{} could not be analyzed: {}", u.path, u.reason),
                ));
            }
            if breaking == 0 && d.unparsed.is_empty() {
                out.push(EvidenceItem::new(
                    p,
                    K::Api,
                    T::Ast,
                    V::Supports,
                    None,
                    format!("no breaking public API change in the promotion ({} Rust file(s) analyzed syntactically)", d.analyzed_files.len()),
                ));
            }
        }
        Err(e) => out.push(EvidenceItem::new(
            p,
            K::Api,
            T::Ast,
            V::Concern,
            None,
            format!("API analysis failed: {e}"),
        )),
    }
}

fn dependency(git: &Git, c: &Candidate, compat: &[CompatResult], out: &mut Vec<EvidenceItem>) {
    let p = "doubledoor.cargo";
    let deps = dependency_delta(git, Some(&c.target), &c.commit, &c.promotes_files);
    for ch in &deps.changes {
        let verdict = match ch.kind {
            CargoChangeKind::DependencyRemoved => V::Concern,
            CargoChangeKind::DependencyChanged if ch.semver_incompatible => V::Concern,
            _ => V::Info,
        };
        out.push(
            EvidenceItem::new(
                p,
                K::Dependency,
                T::Dependency,
                verdict,
                Some(ch.surface.as_str().to_string()),
                format!(
                    "{:?} `{}` in {} ({} → {})",
                    ch.kind,
                    ch.name,
                    ch.manifest,
                    ch.before.as_deref().unwrap_or("-"),
                    ch.after.as_deref().unwrap_or("-")
                ),
            )
            .at(ch.manifest.clone(), None),
        );
    }
    for (m, reason) in &deps.unparsed {
        out.push(EvidenceItem::new(
            p,
            K::Dependency,
            T::Dependency,
            V::Concern,
            Some(format!("file:{m}")),
            format!("{m} could not be parsed: {reason}"),
        ));
    }
    if deps.changes.is_empty() && deps.unparsed.is_empty() {
        out.push(EvidenceItem::new(
            p,
            K::Dependency,
            T::Dependency,
            V::Supports,
            None,
            "the promotion changes no Cargo manifest",
        ));
    }
    // Retained session code relying on canonical changes: settled by the
    // compiler when it ran, otherwise an open concern.
    for e in &c.graph.edges {
        let status = compat
            .iter()
            .find(|r| r.surface == e.from)
            .map(|r| r.status.as_str());
        let (tier, verdict, how) = match (e.breaking, status) {
            (_, Some("incompatible")) => (T::Compiler, V::Blocks, "and no longer compiles"),
            (true, Some("compatible")) => (T::Compiler, V::Supports, "and the compiler accepts it"),
            (true, _) => (
                T::Ast,
                V::Concern,
                "which is API-breaking; not verified by the compiler",
            ),
            (false, _) => (T::Ast, V::Info, "(non-breaking)"),
        };
        out.push(EvidenceItem::new(
            "doubledoor.graph",
            K::Dependency,
            tier,
            verdict,
            Some(e.from.clone()),
            format!("{} relies on canonical change {} {how}", e.from, e.to),
        ));
    }
}

fn diagnostics(v: &ValidationReport, compat: &[CompatResult], out: &mut Vec<EvidenceItem>) {
    let p = "doubledoor.compiler";
    if let Some(g) = v.gates.iter().find(|g| g.name == "cargo check") {
        match &g.status {
            GateStatus::Passed => out.push(
                EvidenceItem::new(
                    p,
                    K::Diagnostic,
                    T::Compiler,
                    V::Supports,
                    None,
                    "`cargo check` of the candidate succeeded",
                )
                .by(&g.argv),
            ),
            GateStatus::Failed { .. } => {
                out.push(
                    EvidenceItem::new(
                        p,
                        K::Diagnostic,
                        T::Compiler,
                        V::Blocks,
                        None,
                        format!("`cargo check` failed with {} error(s)", v.diagnostics.len()),
                    )
                    .by(&g.argv),
                );
                for d in &v.diagnostics {
                    out.push(
                        EvidenceItem::new(
                            p,
                            K::Diagnostic,
                            T::Compiler,
                            V::Blocks,
                            Some(format!("file:{}", d.file)),
                            format!(
                                "{}{}",
                                d.code
                                    .as_deref()
                                    .map(|c| format!("[{c}] "))
                                    .unwrap_or_default(),
                                d.message
                            ),
                        )
                        .at(d.file.clone(), Some(d.line))
                        .by(&g.argv),
                    );
                }
            }
            GateStatus::TimedOut { .. } | GateStatus::Error { .. } => out.push(
                EvidenceItem::new(
                    p,
                    K::Diagnostic,
                    T::Compiler,
                    V::Concern,
                    None,
                    "`cargo check` did not complete",
                )
                .by(&g.argv),
            ),
            GateStatus::Skipped { .. } => {}
        }
    }
    for r in compat {
        let verdict = match r.status.as_str() {
            "compatible" => V::Supports,
            "incompatible" => V::Blocks,
            _ => V::Concern,
        };
        out.push(
            EvidenceItem::new(
                p,
                K::Diagnostic,
                T::Compiler,
                verdict,
                Some(r.surface.clone()),
                format!("retained session code {} is {}", r.surface, r.status),
            )
            .at(r.file.clone(), None)
            .attr("diagnostics", r.diagnostics.join(" | ")),
        );
    }
    for g in v
        .gates
        .iter()
        .filter(|g| !BUILTIN_GATES.contains(&g.name.as_str()))
    {
        let verdict = match (&g.status, g.required) {
            (GateStatus::Passed, _) => V::Supports,
            (GateStatus::Skipped { .. }, _) => continue,
            (_, true) => V::Blocks,
            (_, false) => V::Concern,
        };
        out.push(
            EvidenceItem::new(
                "doubledoor.gates",
                K::Diagnostic,
                T::StaticAnalysis,
                verdict,
                None,
                format!("repository check `{}`", g.name),
            )
            .by(&g.argv),
        );
    }
}

fn behaviour(v: &ValidationReport, logs: Option<&Path>, out: &mut Vec<EvidenceItem>) {
    let p = "doubledoor.tests";
    let Some((idx, g)) = v
        .gates
        .iter()
        .enumerate()
        .find(|(_, g)| g.name == "cargo test")
    else {
        return;
    };
    // The gate's full stdout is in the validation logs (plan index = gate
    // position − 1, after the integrity gate); fall back to the tail.
    let full = logs
        .and_then(|l| {
            std::fs::read_to_string(l.join(format!("{}.out", idx.saturating_sub(1)))).ok()
        })
        .unwrap_or_else(|| g.stdout_tail.clone());
    let (passed, failed) = test_counts(&full);
    let item = match &g.status {
        GateStatus::Passed => EvidenceItem::new(
            p,
            K::Behavioural,
            T::Test,
            V::Supports,
            None,
            format!("`cargo test`: {passed} passed, {failed} failed"),
        ),
        GateStatus::Failed { .. } => EvidenceItem::new(
            p,
            K::Behavioural,
            T::Test,
            V::Blocks,
            None,
            format!("`cargo test` failed: {passed} passed, {failed} failed"),
        ),
        GateStatus::TimedOut { .. } | GateStatus::Error { .. } => EvidenceItem::new(
            p,
            K::Behavioural,
            T::Test,
            V::Concern,
            None,
            "`cargo test` did not complete",
        ),
        GateStatus::Skipped { .. } => return,
    };
    out.push(
        item.attr("passed", passed)
            .attr("failed", failed)
            .by(&g.argv),
    );
}

/// Sum `test result: … N passed; M failed` lines.
pub fn test_counts(out: &str) -> (u64, u64) {
    let mut totals = (0, 0);
    for line in out.lines().filter(|l| l.starts_with("test result:")) {
        let num = |label: &str| {
            line.split(';')
                .find_map(|part| {
                    part.trim()
                        .strip_suffix(label)
                        .and_then(|n| n.trim().rsplit(' ').next()?.parse::<u64>().ok())
                })
                .unwrap_or(0)
        };
        totals.0 += num("passed");
        totals.1 += num("failed");
    }
    totals
}

fn short(s: &str) -> &str {
    &s[..s.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_every_test_binary() {
        let out = "running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n\ntest result: FAILED. 5 passed; 1 failed; 0 ignored\n";
        assert_eq!(test_counts(out), (7, 1));
        assert_eq!(test_counts("nothing"), (0, 0));
    }
}
