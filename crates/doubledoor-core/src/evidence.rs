//! The unified evidence model (§17, §18, §49 Phase V).
//!
//! Every fact Doubledoor or an optional ELCI provider contributes about a
//! reconciliation is an [`EvidenceItem`]: *what* it is about
//! ([`EvidenceKind`]: source, API, dependency, diagnostic, behavioural, …),
//! *how trustworthy* its derivation is ([`EvidenceTier`], the §17
//! resolution hierarchy), *what it says* ([`Verdict`]), and *who* produced
//! it, with the exact command (provenance, DD-I5).
//!
//! The trust root is deterministic (§17): only items at a deterministic
//! tier can decide a unified verdict. Heuristic and model-derived items are
//! kept, labelled, and shown as advisory; they can never declare a
//! reconciliation safe, nor block one. An actor's own claim about its
//! mutation is kept apart from all evidence ([`MutationExplanation`], §18).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What an item of evidence is about. The first five are the kinds the
/// Phase V exit criterion requires a reconciliation report to combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// How the candidate's content came to be (merges, contests, integrity).
    Source,
    /// Public-interface consequences (signatures, semver).
    Api,
    /// Dependency relationships (Cargo, code relying on changed code).
    Dependency,
    /// Compiler and static-analysis findings.
    Diagnostic,
    /// Observed behaviour (tests, runtime checks).
    Behavioural,
    /// Structural/architectural health.
    Architecture,
    /// Where an intent is likely to matter (relevance ranking).
    Relevance,
    /// A proposed repair (never evidence of safety).
    Repair,
}

impl EvidenceKind {
    /// The kinds the §49 exit criterion names.
    pub const REQUIRED: [EvidenceKind; 5] = [
        EvidenceKind::Source,
        EvidenceKind::Api,
        EvidenceKind::Dependency,
        EvidenceKind::Diagnostic,
        EvidenceKind::Behavioural,
    ];
}

/// The §17 resolution hierarchy, most authoritative first. Everything up to
/// and including [`EvidenceTier::Behavioural`] is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceTier {
    GitObject,
    Ast,
    Dependency,
    Compiler,
    StaticAnalysis,
    Test,
    Behavioural,
    Heuristic,
    Model,
}

impl EvidenceTier {
    /// Deterministic tiers may occupy the trust root (§17).
    pub fn is_deterministic(self) -> bool {
        self <= EvidenceTier::Behavioural
    }
}

/// What an item says about the reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Context; neither for nor against.
    Info,
    /// Checked, and nothing wrong was found.
    Supports,
    /// A potential problem that needs a human or agent look.
    Concern,
    /// A demonstrated problem (a compile error, a failing test).
    Blocks,
}

/// One item of evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceItem {
    /// Stable id: `<provider>:<fnv of the item's content>`.
    pub id: String,
    /// `doubledoor.merge`, `doubledoor.ast`, `doubledoor.cargo`,
    /// `doubledoor.compiler`, `doubledoor.tests`, `kaptaind`, `traci`, …
    pub provider: String,
    pub kind: EvidenceKind,
    pub tier: EvidenceTier,
    pub verdict: Verdict,
    /// The surface it is about (`file:…`, `rust:…`, `cargo:…`), if any.
    pub subject: Option<String>,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub summary: String,
    /// Provider-specific structured detail.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    /// The command that produced it (provenance).
    #[serde(default)]
    pub command: Vec<String>,
}

impl EvidenceItem {
    /// Build an item with a content-derived id.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: &str,
        kind: EvidenceKind,
        tier: EvidenceTier,
        verdict: Verdict,
        subject: Option<String>,
        summary: impl Into<String>,
    ) -> Self {
        let summary = summary.into();
        let key = format!(
            "{provider}|{kind:?}|{tier:?}|{verdict:?}|{}|{summary}",
            subject.as_deref().unwrap_or("")
        );
        EvidenceItem {
            id: format!("{provider}:{:016x}", fnv1a(&key)),
            provider: provider.to_string(),
            kind,
            tier,
            verdict,
            subject,
            file: None,
            line: None,
            summary,
            attributes: BTreeMap::new(),
            command: Vec::new(),
        }
    }

    pub fn at(mut self, file: impl Into<String>, line: Option<usize>) -> Self {
        self.file = Some(file.into());
        self.line = line;
        self
    }

    pub fn attr(mut self, k: &str, v: impl ToString) -> Self {
        self.attributes.insert(k.to_string(), v.to_string());
        self
    }

    pub fn by(mut self, argv: &[String]) -> Self {
        self.command = argv.to_vec();
        self
    }
}

/// How a provider's run went. A provider that did not run contributes no
/// evidence — and its absence is reported, never hidden (§21).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProviderStatus {
    Ran {
        items: usize,
    },
    /// Not installed or not configured.
    Unavailable {
        reason: String,
    },
    /// Ran but its output could not be trusted (exit status, parse error,
    /// timeout). Its partial output is discarded.
    Failed {
        reason: String,
    },
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRun {
    pub provider: String,
    /// The strongest tier the provider can contribute (`None` for sinks
    /// such as Padagonia, which store evidence rather than produce it).
    pub tier: Option<EvidenceTier>,
    #[serde(flatten)]
    pub status: ProviderStatus,
    pub duration_ms: u64,
    #[serde(default)]
    pub command: Vec<String>,
}

/// §18: an actor's account of its mutation, kept apart from evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationExplanation {
    /// What the actor says it is doing (its intent) — a claim, not evidence.
    pub actor_claim: Option<String>,
    /// What a heuristic or model inferred about it — not evidence of safety.
    pub inferred_intent: Option<String>,
    /// Ids of deterministic evidence items.
    pub deterministic_evidence: Vec<String>,
    /// Ids of heuristic/model-derived evidence items.
    pub probabilistic_evidence: Vec<String>,
}

/// Two providers disagree about the same subject and kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceConflict {
    pub kind: EvidenceKind,
    pub subject: Option<String>,
    /// `(provider, verdict, item id)` on each side.
    pub positions: Vec<(String, Verdict, String)>,
    pub note: String,
}

/// The unified verdict over deterministic evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnifiedVerdict {
    /// Deterministic evidence covers every required kind; nothing found.
    Clear,
    /// Nothing wrong found, but a required kind has no deterministic
    /// evidence (e.g. tests did not run): unknown is not safe (DD-I8).
    Unverified,
    /// Deterministic evidence raised concerns.
    Concerns,
    /// Deterministic evidence demonstrated a problem.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedAssessment {
    pub verdict: UnifiedVerdict,
    /// Deterministic evidence count per required kind.
    pub coverage: BTreeMap<String, usize>,
    /// Required kinds without deterministic evidence.
    pub unverified_kinds: Vec<EvidenceKind>,
    /// Why the verdict is what it is.
    pub rationale: Vec<String>,
    /// Heuristic/model-derived concerns: shown, never decisive.
    pub advisory: Vec<String>,
}

/// Everything known about one candidate, from every provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundle {
    pub schema: String,
    pub candidate: String,
    pub commit: String,
    pub items: Vec<EvidenceItem>,
    pub providers: Vec<ProviderRun>,
    pub conflicts: Vec<EvidenceConflict>,
    pub explanation: MutationExplanation,
    pub assessment: UnifiedAssessment,
}

/// Schema tag of [`EvidenceBundle`] (and of the external provider protocol).
pub const EVIDENCE_SCHEMA: &str = "doubledoor.evidence/v1";

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_follow_the_section_17_hierarchy() {
        assert!(EvidenceTier::GitObject < EvidenceTier::Ast);
        assert!(EvidenceTier::Compiler < EvidenceTier::StaticAnalysis);
        assert!(EvidenceTier::Behavioural.is_deterministic());
        assert!(!EvidenceTier::Heuristic.is_deterministic());
        assert!(!EvidenceTier::Model.is_deterministic());
    }

    #[test]
    fn item_ids_are_content_derived_and_stable() {
        let a = EvidenceItem::new(
            "traci",
            EvidenceKind::Diagnostic,
            EvidenceTier::StaticAnalysis,
            Verdict::Concern,
            Some("file:a.rs".into()),
            "x",
        );
        let b = EvidenceItem::new(
            "traci",
            EvidenceKind::Diagnostic,
            EvidenceTier::StaticAnalysis,
            Verdict::Concern,
            Some("file:a.rs".into()),
            "x",
        );
        let c = EvidenceItem::new(
            "traci",
            EvidenceKind::Diagnostic,
            EvidenceTier::StaticAnalysis,
            Verdict::Concern,
            Some("file:a.rs".into()),
            "y",
        );
        assert_eq!(a.id, b.id);
        assert_ne!(a.id, c.id);
        assert!(a.id.starts_with("traci:"));
    }

    #[test]
    fn statuses_serialize_tagged() {
        let run = ProviderRun {
            provider: "sore".into(),
            tier: Some(EvidenceTier::Model),
            status: ProviderStatus::Unavailable {
                reason: "not installed".into(),
            },
            duration_ms: 0,
            command: Vec::new(),
        };
        let v = serde_json::to_value(&run).unwrap();
        assert_eq!(v["status"], "unavailable");
        assert_eq!(v["reason"], "not installed");
        assert_eq!(serde_json::from_value::<ProviderRun>(v).unwrap(), run);
    }
}
