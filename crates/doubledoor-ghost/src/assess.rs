//! Surface-level assessment of one ghost against one session (§11, §12, §47).
//!
//! Three evidence layers, each producing per-surface [`SurfaceAssessment`]s:
//!
//! 1. **File** (Phase II): the ghost's touched files against the session's
//!    declared read/write/dependency sets — plus, in Phase III, the files the
//!    session actually changed in its workspace (a derived write set).
//! 2. **Symbol** (Phase III): each symbol the ghost changed against the
//!    session's own changes and references. This layer sees what Git's line
//!    analysis cannot: a signature change in one file that invalidates a
//!    call in another file the ghost never touched.
//! 3. **Cargo** (Phase III): structural manifest changes against the crates
//!    the session touched and the external crates its code uses.
//!
//! The ghost's classification is the strongest per-surface class. Analysis
//! failure that could hide a relevant change yields `Unknown`, which is
//! never safe (§12, DD-I8).

use serde::{Deserialize, Serialize};

use doubledoor_core::{AssumptionViolation, DoorSession, SurfaceId, SurfaceSet};
use doubledoor_surface::cargo::{CargoChangeKind, DependencyDelta};
use doubledoor_surface::rust::SymbolKind;
use doubledoor_surface::semantic::{
    ApiImpact, ChangeKind, Relation, SemanticDelta, SessionReference, SessionSemantics,
    SymbolChange,
};
use doubledoor_surface::FileSurface;

use crate::classify::GhostClassification;

/// How one surface touched by a ghost relates to the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceAssessment {
    pub surface: SurfaceId,
    pub classification: GhostClassification,
    /// Why, in terms an actor can check (§32).
    pub reason: String,
}

/// The full result of assessing a ghost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub classification: GhostClassification,
    pub assessments: Vec<SurfaceAssessment>,
    pub violations: Vec<AssumptionViolation>,
    /// Surfaces that make the ghost matter (non-independent assessments).
    pub intersecting: SurfaceSet,
    /// Evidence notes (analysis limits, fallbacks).
    pub notes: Vec<String>,
}

/// Severity order used to pick the ghost's overall class.
pub fn rank(c: GhostClassification) -> u8 {
    match c {
        GhostClassification::Independent => 0,
        GhostClassification::DependencyRelevant => 1,
        GhostClassification::ReadRelevant => 2,
        GhostClassification::WriteOverlap => 3,
        GhostClassification::ApiBreaking => 4,
        GhostClassification::InvariantThreat => 5,
        GhostClassification::Unknown => 6,
    }
}

/// Assess a ghost. `touched` is the ghost's file surfaces; `semantics` is the
/// session's derived footprint (`None` when it could not be computed);
/// `analysis_ok` is false when the textual/file analysis itself failed.
pub fn assess(
    session: &DoorSession,
    semantics: Option<&SessionSemantics>,
    touched: &SurfaceSet,
    semantic: &SemanticDelta,
    deps: &DependencyDelta,
    analysis_ok: bool,
) -> Assessment {
    let mut out = Assessment {
        classification: GhostClassification::Independent,
        assessments: Vec::new(),
        violations: Vec::new(),
        intersecting: SurfaceSet::new(),
        notes: Vec::new(),
    };

    // 1. Files.
    let derived_writes: SurfaceSet = semantics
        .map(|s| {
            s.changed_files
                .iter()
                .filter_map(|p| FileSurface::new(p.as_str()).ok().map(|f| f.id))
                .collect()
        })
        .unwrap_or_default();
    for surface in touched.iter() {
        let (class, reason) = if session.write_set.contains(surface) {
            (
                GhostClassification::WriteOverlap,
                "in the session's declared write set",
            )
        } else if derived_writes.contains(surface) {
            (
                GhostClassification::WriteOverlap,
                "the session has also changed this file",
            )
        } else if session.read_set.contains(surface) {
            (
                GhostClassification::ReadRelevant,
                "in the session's declared read set",
            )
        } else if session.dependency_set.contains(surface) {
            (
                GhostClassification::DependencyRelevant,
                "in the session's declared dependency set",
            )
        } else {
            (
                GhostClassification::Independent,
                "no file-level overlap with the session",
            )
        };
        out.push(surface.clone(), class, reason.to_string());
    }

    // 2. Symbols.
    match semantics {
        Some(sem) => {
            for change in &semantic.changes {
                assess_symbol(sem, change, &mut out);
            }
        }
        None if !semantic.changes.is_empty() => {
            let breaking = semantic
                .changes
                .iter()
                .any(|c| c.impact == ApiImpact::Breaking);
            for change in &semantic.changes {
                let (class, reason) = if breaking {
                    (
                        GhostClassification::Unknown,
                        "the session's symbol footprint could not be analyzed, so this API change cannot be ruled out".to_string(),
                    )
                } else {
                    (
                        GhostClassification::Independent,
                        "non-breaking change; the session's symbol footprint was unavailable"
                            .to_string(),
                    )
                };
                out.push(change.symbol.clone(), class, reason);
            }
        }
        None => {}
    }

    // 3. Cargo.
    for c in &deps.changes {
        let dir = doubledoor_surface::cratemap::manifest_dir(&c.manifest);
        let Some(sem) = semantics else {
            out.push(
                c.surface.clone(),
                GhostClassification::Unknown,
                "manifest changed; the session's footprint could not be analyzed".to_string(),
            );
            continue;
        };
        let workspace_level = c.section.starts_with("workspace.");
        let relevant =
            sem.touches_crate_dir(&dir) || (workspace_level && !sem.crate_dirs.is_empty());
        let uses = c.is_dependency() && sem.external_crates.contains(&c.code_name());
        let (class, reason) = match c.kind {
            CargoChangeKind::DependencyRemoved if uses && relevant => {
                out.violations.push(AssumptionViolation::new(
                    c.surface.clone(),
                    format!(
                        "dependency `{}` was removed from {}, but the session's code uses `{}`",
                        c.name,
                        c.manifest,
                        c.code_name()
                    ),
                ));
                (
                    GhostClassification::ApiBreaking,
                    format!(
                        "removes dependency `{}`, which the session's code uses",
                        c.name
                    ),
                )
            }
            CargoChangeKind::DependencyChanged if uses && relevant && c.semver_incompatible => {
                out.violations.push(AssumptionViolation::new(
                    c.surface.clone(),
                    format!(
                        "dependency `{}` moved across a semver boundary ({} → {}), and the session's code uses it",
                        c.name,
                        c.before.as_deref().unwrap_or("?"),
                        c.after.as_deref().unwrap_or("?")
                    ),
                ));
                (
                    GhostClassification::ApiBreaking,
                    format!(
                        "semver-incompatible change of `{}`, which the session uses",
                        c.name
                    ),
                )
            }
            _ if relevant => (
                GhostClassification::DependencyRelevant,
                format!(
                    "{:?} `{}` in a manifest governing the session's code",
                    c.kind, c.name
                ),
            ),
            _ => (
                GhostClassification::Independent,
                "manifest of a package the session does not touch".to_string(),
            ),
        };
        out.push(c.surface.clone(), class, reason);
    }

    // Analysis gaps that could hide a relevant change are Unknown (§12).
    if !analysis_ok {
        out.classification = GhostClassification::Unknown;
        out.notes.push("textual analysis incomplete".to_string());
    }
    if let Some(sem) = semantics {
        for u in &semantic.unparsed {
            if in_touched_crate(sem, &u.path) {
                out.push(
                    FileSurface::new(u.path.as_str())
                        .map(|f| f.id)
                        .unwrap_or_else(|_| SurfaceId::new(u.path.clone()).expect("non-empty")),
                    GhostClassification::Unknown,
                    format!(
                        "{} version could not be analyzed ({}) in a crate the session touches",
                        u.side, u.reason
                    ),
                );
            }
        }
        for (manifest, reason) in &deps.unparsed {
            let dir = doubledoor_surface::cratemap::manifest_dir(manifest);
            if sem.touches_crate_dir(&dir) {
                out.notes
                    .push(format!("{manifest} could not be parsed: {reason}"));
                out.classification = GhostClassification::Unknown;
            }
        }
        if !sem.unparsed.is_empty()
            && semantic
                .changes
                .iter()
                .any(|c| in_touched_crate(sem, &c.file))
        {
            out.notes.push(format!(
                "session files could not be analyzed: {}",
                sem.unparsed
                    .iter()
                    .map(|u| u.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            out.classification = GhostClassification::Unknown;
        }
        if !sem.fallback_files.is_empty() {
            out.notes.push(format!(
                "session working copy did not parse; analyzed committed versions of: {}",
                sem.fallback_files.join(", ")
            ));
        }
    }
    for (manifest, reason) in &deps.unparsed {
        out.notes
            .push(format!("manifest {manifest} not analyzed: {reason}"));
    }

    for a in &out.assessments {
        if rank(a.classification) > rank(out.classification) {
            out.classification = a.classification;
        }
        if a.classification != GhostClassification::Independent {
            out.intersecting.insert(a.surface.clone());
        }
    }
    out
}

impl Assessment {
    fn push(&mut self, surface: SurfaceId, classification: GhostClassification, reason: String) {
        self.assessments.push(SurfaceAssessment {
            surface,
            classification,
            reason,
        });
    }
}

fn in_touched_crate(sem: &SessionSemantics, path: &str) -> bool {
    sem.crate_dirs
        .iter()
        .any(|d| d.is_empty() || path.starts_with(&format!("{d}/")))
}

/// Symbol layer: one changed symbol against the session's footprint.
fn assess_symbol(sem: &SessionSemantics, change: &SymbolChange, out: &mut Assessment) {
    let q = change.qualified();
    if sem.modifies(change) {
        let api = if change.impact == ApiImpact::Breaking {
            format!(
                "; the ghost's change is API-breaking ({})",
                change.describe()
            )
        } else {
            String::new()
        };
        out.push(
            change.symbol.clone(),
            GhostClassification::WriteOverlap,
            format!("both the ghost and the session changed `{q}`{api}"),
        );
        return;
    }
    let rels = sem.relations(change);
    let with = |r: Relation| -> Vec<&SessionReference> {
        rels.iter()
            .filter(|(x, _)| *x == r)
            .map(|(_, s)| *s)
            .collect()
    };
    let direct = with(Relation::Direct);
    let owner = with(Relation::Owner);
    let literal = with(Relation::StructLiteral);
    let exhaustive = with(Relation::ExhaustiveMatch);
    let implementor = with(Relation::TraitImpl);

    // (breaking sites, why) — the session relies on something the change removes.
    let breaking: Option<(Vec<&SessionReference>, String)> = match (&change.change, change.kind) {
        (ChangeKind::Added, SymbolKind::Field) if !literal.is_empty() => Some((
            literal.clone(),
            format!("struct literals of the owner must now initialize the new field `{q}`"),
        )),
        (ChangeKind::Added, SymbolKind::Variant) if !exhaustive.is_empty() => Some((
            exhaustive.clone(),
            format!("exhaustive matches on the owner do not cover the new variant `{q}`"),
        )),
        (ChangeKind::Added, SymbolKind::TraitItem)
            if !change.has_default && !implementor.is_empty() =>
        {
            Some((
                implementor.clone(),
                format!("implementations of the trait must now provide `{q}`"),
            ))
        }
        (_, _) if change.impact == ApiImpact::Breaking => {
            let mut sites = direct.clone();
            if change.kind == SymbolKind::Field {
                sites.extend(literal.iter().copied());
            }
            if change.kind == SymbolKind::TraitItem {
                sites.extend(implementor.iter().copied());
            }
            (!sites.is_empty()).then(|| (sites, format!("`{q}` {}", change.describe())))
        }
        _ => None,
    };
    if let Some((sites, why)) = breaking {
        out.violations.push(AssumptionViolation::new(
            change.symbol.clone(),
            format!("{why}; the session relies on it at {}", sites_text(&sites)),
        ));
        out.push(change.symbol.clone(), GhostClassification::ApiBreaking, why);
        return;
    }

    let users: Vec<&SessionReference> = direct
        .iter()
        .chain(owner.iter())
        .chain(literal.iter())
        .chain(exhaustive.iter())
        .chain(implementor.iter())
        .copied()
        .collect();
    if users.is_empty() {
        out.push(
            change.symbol.clone(),
            GhostClassification::Independent,
            format!("`{q}` {}; the session does not use it", change.describe()),
        );
    } else {
        out.push(
            change.symbol.clone(),
            GhostClassification::DependencyRelevant,
            format!(
                "`{q}` {}; the session uses it at {}",
                change.describe(),
                sites_text(&users)
            ),
        );
    }
}

/// `app::handler::handle (src/handler.rs:12)`, first three, then a count.
fn sites_text(sites: &[&SessionReference]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let unique: Vec<String> = sites
        .iter()
        .map(|s| format!("{} ({}:{})", s.reference.from, s.file, s.reference.line))
        .filter(|s| seen.insert(s.clone()))
        .collect();
    let shown = unique
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if unique.len() > 3 {
        format!("{shown} and {} more", unique.len() - 3)
    } else {
        shown
    }
}
