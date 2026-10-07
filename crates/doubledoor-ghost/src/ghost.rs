//! The GhostDiff record (§12): one observed external commit, its parsed
//! delta, its surface impact, and its classification against a session.

use serde::{Deserialize, Serialize};

use doubledoor_core::{
    ActorId, AssumptionViolation, DoorSessionId, GhostDiffId, GitOid, IntentDescriptor, SurfaceSet,
};

use doubledoor_surface::cargo::DependencyDelta;
use doubledoor_surface::semantic::{ApiDelta, SemanticDelta};

use crate::assess::SurfaceAssessment;
use crate::classify::GhostClassification;
use crate::diff::TextualDelta;

/// A single ghost observation: an external commit the session's actor did
/// not make, captured and classified against the session's speculative work.
///
/// Persisted per session as `.doubledoor/sessions/<id>/ghosts/<ghost_id>.json`
/// inside a schema-versioned [`doubledoor_protocol::Envelope`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GhostDiff {
    pub id: GhostDiffId,
    pub session: DoorSessionId,

    /// The commit's author mapped to a Doubledoor actor id, when it could be
    /// determined — a `dd-actor:` trailer takes precedence over the git
    /// author identity, and unparseable identities yield `None`.
    pub source_actor: Option<ActorId>,

    /// The observed (external) commit.
    pub source_commit: GitOid,

    /// The commit's first parent — the baseline the delta was computed
    /// against. For root commits this is the null (all-zeros) object id, and
    /// the delta was computed against the empty tree.
    pub parent_commit: GitOid,

    /// The actor's stated intent, from a `dd-intent:` trailer when present.
    pub intent: Option<IntentDescriptor>,

    /// The parsed unified diff of the commit (hunk headers only).
    pub textual: TextualDelta,

    /// Symbol-level changes of the commit's Rust files (Phase III).
    /// Defaulted so Phase II records still load.
    #[serde(default)]
    pub semantic: SemanticDelta,

    /// The public-API view of `semantic`: breaking and additive changes to
    /// `pub` items (Phase III).
    #[serde(default)]
    pub api: ApiDelta,

    /// Structural `Cargo.toml` changes (Phase III).
    #[serde(default)]
    pub dependencies: DependencyDelta,

    /// Per-surface assessment against the session: files, symbols, and
    /// manifest entries, each with its class and reason (Phase III).
    #[serde(default)]
    pub assessments: Vec<SurfaceAssessment>,

    /// Every surface the commit touched, in `file:<path>` id form.
    pub affected_surfaces: SurfaceSet,

    /// The surfaces that make this ghost matter: file-level intersections
    /// with the session's sets, plus every symbol or manifest surface whose
    /// assessment is not `Independent`.
    pub intersecting_surfaces: SurfaceSet,

    /// Assumptions the session's work relies on that this ghost breaks
    /// (Phase III): a changed signature the session calls, a removed
    /// dependency its code uses, a new field its struct literals omit.
    pub invalidated_assumptions: Vec<AssumptionViolation>,

    /// The §12 classification from the file-level classifier.
    pub classification: GhostClassification,

    /// Human-auditable provenance: object ids and the exact git commands (or
    /// failure notes) behind every derived field.
    pub evidence: Vec<String>,
}
