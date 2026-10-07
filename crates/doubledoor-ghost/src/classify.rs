//! Ghost classification (§12, Phase II).
//!
//! The Phase II classifier is file-level (§11): the ghost commit's touched
//! surfaces (W_m) are compared against the session's write set (W_s), read
//! set (R_s), and dependency set (D_s):
//!
//! ```text
//! W_m ∩ W_s ≠ ∅            → WriteOverlap       (concurrent edit hazard)
//! W_m ∩ R_s ≠ ∅            → ReadRelevant       (basis of work may have moved)
//! W_m ∩ D_s ≠ ∅            → DependencyRelevant (structural input changed)
//! otherwise                → Independent
//! analysis failure         → Unknown            (never means safe, §12)
//! ```
//!
//! `ApiBreaking` and `InvariantThreat` are reserved for Phase III's semantic
//! analysis and are never emitted by this classifier.

use serde::{Deserialize, Serialize};

use doubledoor_core::{DoorSession, SurfaceSet};

/// How a ghost commit relates to a session's speculative work (§12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GhostClassification {
    /// No touched surface intersects any session set; nothing to assess.
    Independent,
    /// A touched surface is in the session's read set.
    ReadRelevant,
    /// A touched surface is in the session's dependency set.
    DependencyRelevant,
    /// A touched surface is in the session's write set.
    WriteOverlap,
    /// Reserved for Phase III: an API the session relies on changed incompatibly.
    ApiBreaking,
    /// Reserved for Phase III: a change threatens a stated invariant.
    InvariantThreat,
    /// Analysis could not complete (oversized/malformed diff, git failure).
    /// Never means safe (§12) — the actor must assess manually.
    Unknown,
}

impl std::fmt::Display for GhostClassification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            GhostClassification::Independent => "independent",
            GhostClassification::ReadRelevant => "read-relevant",
            GhostClassification::DependencyRelevant => "dependency-relevant",
            GhostClassification::WriteOverlap => "write-overlap",
            GhostClassification::ApiBreaking => "api-breaking",
            GhostClassification::InvariantThreat => "invariant-threat",
            GhostClassification::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

impl GhostClassification {
    /// The snake_case wire name used on the event stream (§13), e.g.
    /// `dependency_relevant`.
    pub fn wire_name(self) -> &'static str {
        match self {
            GhostClassification::Independent => "independent",
            GhostClassification::ReadRelevant => "read_relevant",
            GhostClassification::DependencyRelevant => "dependency_relevant",
            GhostClassification::WriteOverlap => "write_overlap",
            GhostClassification::ApiBreaking => "api_breaking",
            GhostClassification::InvariantThreat => "invariant_threat",
            GhostClassification::Unknown => "unknown",
        }
    }

    /// Whether the actor must assess the ghost before the session can
    /// continue. Only `Independent` is exempt; `Unknown` is never safe (§12).
    pub fn needs_assessment(self) -> bool {
        self != GhostClassification::Independent
    }
}

/// Classify one observation against a session. `touched` is the set of
/// surfaces the ghost commit changed (W_m). Precedence is WriteOverlap >
/// ReadRelevant > DependencyRelevant > Independent: the classification names
/// the strongest relationship the observation has to the session.
pub fn classify(touched: &SurfaceSet, session: &DoorSession) -> GhostClassification {
    if !touched.is_disjoint(&session.write_set) {
        GhostClassification::WriteOverlap
    } else if !touched.is_disjoint(&session.read_set) {
        GhostClassification::ReadRelevant
    } else if !touched.is_disjoint(&session.dependency_set) {
        GhostClassification::DependencyRelevant
    } else {
        GhostClassification::Independent
    }
}

/// Classify one observation, folding analysis health into the result (§12):
/// any analysis failure (oversized or malformed diff, unparseable git
/// output) yields `Unknown` regardless of surface overlap. Callers pass
/// `analysis_ok: false` when any step of the observation failed; `Unknown`
/// must never be treated as safe.
pub fn classify_observation(
    touched: &SurfaceSet,
    session: &DoorSession,
    analysis_ok: bool,
) -> GhostClassification {
    if !analysis_ok {
        return GhostClassification::Unknown;
    }
    classify(touched, session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use doubledoor_core::{
        ActorId, CanonicalState, GitOid, IntentDescriptor, PolicySnapshot, SurfaceId,
    };
    use std::time::SystemTime;

    fn session_with(read: &[&str], write: &[&str], deps: &[&str]) -> DoorSession {
        let mut s = DoorSession::new(
            ActorId::new("codex-7").unwrap(),
            IntentDescriptor::new("test intent"),
            CanonicalState {
                commit: GitOid::parse(&"a".repeat(40)).unwrap(),
                tree: GitOid::parse(&"b".repeat(40)).unwrap(),
                captured_at: SystemTime::UNIX_EPOCH,
            },
            Vec::new(),
            PolicySnapshot::default(),
        );
        for r in read {
            s.read_set.insert(SurfaceId::new(*r).unwrap());
        }
        for w in write {
            s.write_set.insert(SurfaceId::new(*w).unwrap());
        }
        for d in deps {
            s.dependency_set.insert(SurfaceId::new(*d).unwrap());
        }
        s
    }

    fn touched(paths: &[&str]) -> SurfaceSet {
        paths.iter().map(|p| SurfaceId::new(*p).unwrap()).collect()
    }

    #[test]
    fn write_overlap_beats_read_relevance() {
        let s = session_with(&["file:src/shared.rs"], &["file:src/mine.rs"], &[]);
        let t = touched(&["file:src/mine.rs", "file:src/shared.rs"]);
        assert_eq!(classify(&t, &s), GhostClassification::WriteOverlap);
    }

    #[test]
    fn read_relevance_when_only_read_set_intersects() {
        let s = session_with(&["file:src/shared.rs"], &["file:src/mine.rs"], &[]);
        let t = touched(&["file:src/shared.rs"]);
        assert_eq!(classify(&t, &s), GhostClassification::ReadRelevant);
    }

    #[test]
    fn dependency_relevance_when_only_dependency_set_intersects() {
        let s = session_with(&[], &["file:src/mine.rs"], &["file:Cargo.toml"]);
        let t = touched(&["file:Cargo.toml"]);
        assert_eq!(classify(&t, &s), GhostClassification::DependencyRelevant);
    }

    #[test]
    fn disjoint_commit_is_independent() {
        let s = session_with(&["file:src/a.rs"], &["file:src/b.rs"], &["file:src/c.rs"]);
        let t = touched(&["file:src/other.rs", "file:docs/new.md"]);
        assert_eq!(classify(&t, &s), GhostClassification::Independent);
    }

    #[test]
    fn empty_session_sets_make_everything_independent() {
        let s = session_with(&[], &[], &[]);
        let t = touched(&["file:anything.rs"]);
        assert_eq!(classify(&t, &s), GhostClassification::Independent);
    }

    #[test]
    fn analysis_failure_is_unknown_even_on_overlap() {
        // §12: Unknown must never mean safe — even a write-overlapping commit
        // is Unknown when analysis failed (oversized/malformed diff).
        let s = session_with(&[], &["file:src/mine.rs"], &[]);
        let t = touched(&["file:src/mine.rs"]);
        assert_eq!(
            classify_observation(&t, &s, false),
            GhostClassification::Unknown
        );
        assert_eq!(
            classify_observation(&t, &s, true),
            GhostClassification::WriteOverlap
        );
    }

    #[test]
    fn analysis_failure_on_disjoint_commit_is_still_unknown() {
        let s = session_with(&[], &[], &[]);
        let t = touched(&["file:far-away.rs"]);
        assert_eq!(
            classify_observation(&t, &s, false),
            GhostClassification::Unknown
        );
    }

    #[test]
    fn only_independent_is_exempt_from_assessment() {
        assert!(!GhostClassification::Independent.needs_assessment());
        for c in [
            GhostClassification::ReadRelevant,
            GhostClassification::DependencyRelevant,
            GhostClassification::WriteOverlap,
            GhostClassification::ApiBreaking,
            GhostClassification::InvariantThreat,
            GhostClassification::Unknown,
        ] {
            assert!(c.needs_assessment(), "{c} must need assessment");
        }
        assert_eq!(
            GhostClassification::DependencyRelevant.wire_name(),
            "dependency_relevant"
        );
    }

    #[test]
    fn phase_ii_never_emits_phase_iii_classes() {
        // Every reachable Phase II outcome is one of the four file-level
        // classes; ApiBreaking/InvariantThreat exist only for Phase III.
        let sets = [
            (vec!["file:r.rs"], vec!["file:w.rs"], vec!["file:d.rs"]),
            (vec![], vec!["file:w.rs"], vec![]),
            (vec!["file:r.rs"], vec![], vec![]),
            (vec![], vec![], vec!["file:d.rs"]),
            (vec![], vec![], vec![]),
        ];
        let probes: [&[&str]; 4] = [
            &["file:w.rs"],
            &["file:r.rs"],
            &["file:d.rs"],
            &["file:unrelated.rs"],
        ];
        for (r, w, d) in sets {
            let s = session_with(&r, &w, &d);
            for probe in probes {
                let got = classify_observation(&touched(probe), &s, true);
                assert!(
                    matches!(
                        got,
                        GhostClassification::Independent
                            | GhostClassification::ReadRelevant
                            | GhostClassification::DependencyRelevant
                            | GhostClassification::WriteOverlap
                    ),
                    "Phase II emitted a Phase III classification: {got}"
                );
            }
        }
    }
}
