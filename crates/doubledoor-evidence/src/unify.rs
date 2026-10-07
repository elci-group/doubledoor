//! Combining evidence (§17, §18): the unified verdict, provider conflicts,
//! and the separation of claims, deterministic evidence, and probabilistic
//! evidence.
//!
//! Rules:
//! - only deterministic items decide the verdict: any `blocks` → BLOCKED;
//!   else any `concern` → CONCERNS; else a required kind (source, API,
//!   dependency, diagnostic, behavioural) with no deterministic check →
//!   UNVERIFIED; else CLEAR;
//! - heuristic/model concerns are listed as advisory and change nothing —
//!   probabilistic reasoning never declares safety, nor blocks (§17);
//! - where providers answer the same question differently — two
//!   deterministic items about the same API or dependency subject (or one
//!   about the whole candidate), one `supports`, the other `concern`/
//!   `blocks` — the disagreement is recorded, not resolved by fiat. The
//!   stricter item still counts toward the verdict.

use std::collections::BTreeMap;

use doubledoor_core::evidence::{
    EvidenceConflict, EvidenceItem, EvidenceKind, MutationExplanation, UnifiedAssessment,
    UnifiedVerdict, Verdict,
};

pub fn assess(items: &[EvidenceItem]) -> UnifiedAssessment {
    let det: Vec<&EvidenceItem> = items.iter().filter(|i| i.tier.is_deterministic()).collect();
    let mut coverage = BTreeMap::new();
    let mut unverified = Vec::new();
    for kind in EvidenceKind::REQUIRED {
        let n = det
            .iter()
            .filter(|i| i.kind == kind && i.verdict != Verdict::Info)
            .count();
        coverage.insert(format!("{kind:?}").to_lowercase(), n);
        if n == 0 {
            unverified.push(kind);
        }
    }
    let blockers: Vec<&&EvidenceItem> = det
        .iter()
        .filter(|i| i.verdict == Verdict::Blocks)
        .collect();
    let concerns: Vec<&&EvidenceItem> = det
        .iter()
        .filter(|i| i.verdict == Verdict::Concern)
        .collect();
    let verdict = if !blockers.is_empty() {
        UnifiedVerdict::Blocked
    } else if !concerns.is_empty() {
        UnifiedVerdict::Concerns
    } else if !unverified.is_empty() {
        UnifiedVerdict::Unverified
    } else {
        UnifiedVerdict::Clear
    };
    let mut rationale = Vec::new();
    for i in &blockers {
        rationale.push(format!("blocks [{}]: {}", i.provider, i.summary));
    }
    for i in &concerns {
        rationale.push(format!("concern [{}]: {}", i.provider, i.summary));
    }
    for k in &unverified {
        rationale.push(format!("no deterministic {k:?} evidence").to_lowercase());
    }
    let advisory: Vec<String> = items
        .iter()
        .filter(|i| !i.tier.is_deterministic() && i.verdict >= Verdict::Concern)
        .map(|i| format!("[{} · {:?}] {}", i.provider, i.tier, i.summary).to_lowercase())
        .collect();
    rationale.push(format!(
        "trust root: {} deterministic item(s); {} heuristic/model item(s) are advisory only",
        det.len(),
        items.len() - det.len()
    ));
    UnifiedAssessment {
        verdict,
        coverage,
        unverified_kinds: unverified,
        rationale,
        advisory,
    }
}

pub fn conflicts(items: &[EvidenceItem]) -> Vec<EvidenceConflict> {
    let mut out: Vec<EvidenceConflict> = Vec::new();
    let comparable = |i: &&EvidenceItem| {
        i.tier.is_deterministic() && matches!(i.kind, EvidenceKind::Api | EvidenceKind::Dependency)
    };
    let pro: Vec<&EvidenceItem> = items
        .iter()
        .filter(comparable)
        .filter(|i| i.verdict == Verdict::Supports)
        .collect();
    let con: Vec<&EvidenceItem> = items
        .iter()
        .filter(comparable)
        .filter(|i| i.verdict >= Verdict::Concern)
        .collect();
    for p in &pro {
        for c in &con {
            if p.kind != c.kind || provider_family(&p.provider) == provider_family(&c.provider) {
                continue;
            }
            let overlap = p.subject.is_none() || c.subject.is_none() || p.subject == c.subject;
            if !overlap {
                continue;
            }
            let subject = c.subject.clone().or_else(|| p.subject.clone());
            if let Some(existing) = out
                .iter_mut()
                .find(|x| x.kind == p.kind && x.positions.iter().any(|(_, _, id)| id == &p.id))
            {
                existing
                    .positions
                    .push((c.provider.clone(), c.verdict, c.id.clone()));
                continue;
            }
            out.push(EvidenceConflict {
                kind: p.kind,
                subject,
                positions: vec![
                    (p.provider.clone(), p.verdict, p.id.clone()),
                    (c.provider.clone(), c.verdict, c.id.clone()),
                ],
                note: format!(
                    "{} says: {} — but {} says: {}",
                    p.provider, p.summary, c.provider, c.summary
                ),
            });
        }
    }
    out
}

/// `doubledoor.*` providers are one family: Doubledoor does not "disagree"
/// with itself across layers (the compiler settling an AST concern is
/// refinement, not conflict).
fn provider_family(p: &str) -> &str {
    p.split('.').next().unwrap_or(p)
}

/// §18: the actor's claim, the (heuristically) inferred intent, and the ids
/// of deterministic vs probabilistic evidence — never merged.
pub fn explanation(actor_claim: &str, items: &[EvidenceItem]) -> MutationExplanation {
    let inferred = items
        .iter()
        .filter(|i| i.kind == EvidenceKind::Relevance)
        .take(3)
        .filter_map(|i| {
            i.subject
                .as_deref()
                .map(|s| s.trim_start_matches("dir:").to_string())
        })
        .collect::<Vec<_>>();
    MutationExplanation {
        actor_claim: Some(actor_claim.to_string()).filter(|s| !s.is_empty()),
        inferred_intent: (!inferred.is_empty()).then(|| {
            format!(
                "heuristically most relevant to: {} (not evidence of safety)",
                inferred.join(", ")
            )
        }),
        deterministic_evidence: items
            .iter()
            .filter(|i| i.tier.is_deterministic())
            .map(|i| i.id.clone())
            .collect(),
        probabilistic_evidence: items
            .iter()
            .filter(|i| !i.tier.is_deterministic())
            .map(|i| i.id.clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use doubledoor_core::evidence::EvidenceTier as T;

    fn item(p: &str, k: EvidenceKind, t: T, v: Verdict, s: Option<&str>) -> EvidenceItem {
        EvidenceItem::new(p, k, t, v, s.map(String::from), format!("{p} {v:?}"))
    }

    fn full_coverage() -> Vec<EvidenceItem> {
        EvidenceKind::REQUIRED
            .iter()
            .map(|k| item("doubledoor.x", *k, T::Compiler, Verdict::Supports, None))
            .collect()
    }

    #[test]
    fn verdict_comes_only_from_deterministic_evidence() {
        let mut items = full_coverage();
        assert_eq!(assess(&items).verdict, UnifiedVerdict::Clear);
        // A model claiming a block is advisory; it cannot block…
        items.push(item(
            "sore",
            EvidenceKind::Repair,
            T::Model,
            Verdict::Blocks,
            None,
        ));
        let a = assess(&items);
        assert_eq!(a.verdict, UnifiedVerdict::Clear);
        assert_eq!(a.advisory.len(), 1);
        // …and a heuristic "supports" cannot fill a coverage gap.
        let mut gap: Vec<EvidenceItem> = full_coverage()
            .into_iter()
            .filter(|i| i.kind != EvidenceKind::Behavioural)
            .collect();
        gap.push(item(
            "zoc",
            EvidenceKind::Behavioural,
            T::Heuristic,
            Verdict::Supports,
            None,
        ));
        let a = assess(&gap);
        assert_eq!(a.verdict, UnifiedVerdict::Unverified);
        assert_eq!(a.unverified_kinds, [EvidenceKind::Behavioural]);
    }

    #[test]
    fn precedence_blocked_then_concerns_then_unverified() {
        let mut items = vec![item(
            "doubledoor.compiler",
            EvidenceKind::Diagnostic,
            T::Compiler,
            Verdict::Concern,
            None,
        )];
        assert_eq!(assess(&items).verdict, UnifiedVerdict::Concerns);
        items.push(item(
            "doubledoor.tests",
            EvidenceKind::Behavioural,
            T::Test,
            Verdict::Blocks,
            None,
        ));
        assert_eq!(assess(&items).verdict, UnifiedVerdict::Blocked);
        assert_eq!(assess(&[]).verdict, UnifiedVerdict::Unverified);
    }

    #[test]
    fn cross_provider_disagreement_is_recorded() {
        let items = vec![
            item(
                "kaptaind",
                EvidenceKind::Api,
                T::StaticAnalysis,
                Verdict::Supports,
                None,
            ),
            item(
                "doubledoor.ast",
                EvidenceKind::Api,
                T::Ast,
                Verdict::Concern,
                Some("rust:a::f"),
            ),
            // Same family: refinement, not conflict.
            item(
                "doubledoor.compiler",
                EvidenceKind::Dependency,
                T::Compiler,
                Verdict::Supports,
                Some("rust:a::g"),
            ),
            item(
                "doubledoor.graph",
                EvidenceKind::Dependency,
                T::Ast,
                Verdict::Concern,
                Some("rust:a::g"),
            ),
        ];
        let c = conflicts(&items);
        assert_eq!(c.len(), 1, "{c:#?}");
        assert_eq!(c[0].kind, EvidenceKind::Api);
        assert_eq!(c[0].subject.as_deref(), Some("rust:a::f"));
    }

    #[test]
    fn explanation_separates_claims_from_evidence() {
        let items = vec![
            item(
                "doubledoor.ast",
                EvidenceKind::Api,
                T::Ast,
                Verdict::Supports,
                None,
            ),
            item(
                "zoc",
                EvidenceKind::Relevance,
                T::Heuristic,
                Verdict::Info,
                Some("dir:crates/x"),
            ),
        ];
        let e = explanation("add retries", &items);
        assert_eq!(e.actor_claim.as_deref(), Some("add retries"));
        assert!(e.inferred_intent.unwrap().contains("crates/x"));
        assert_eq!(e.deterministic_evidence.len(), 1);
        assert_eq!(e.probabilistic_evidence.len(), 1);
    }
}
