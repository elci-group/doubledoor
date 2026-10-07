//! Structural JSON view types for the CLI (`--format json`) and future IPC.
//!
//! These are deliberately flat, owned, and serialization-stable: human
//! rendering lives in the CLI, everything crossing a boundary lives here
//! (§31/§32 of the directive).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use doubledoor_core::{ActorId, DoorSessionId, DoorState, GhostDiffId, GitOid};

/// Serializable summary of one DoorSession for `sessions`/`status`/`inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: DoorSessionId,
    pub actor: ActorId,
    pub intent: String,
    pub state: DoorState,
    pub base_commit: GitOid,
    pub workspace: Option<PathBuf>,
    pub created_at_millis: u64,
    pub requested_surfaces: Vec<String>,
    /// §11 sets (Phase II). Defaulted so Phase I consumers still parse.
    #[serde(default)]
    pub read_set: Vec<String>,
    #[serde(default)]
    pub write_set: Vec<String>,
    #[serde(default)]
    pub dependency_set: Vec<String>,
}

/// Aggregate view for `doubledoor status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub repo_root: PathBuf,
    /// Canonical `HEAD` (Phase VII: remotes compare it before hosting
    /// sessions or importing canonical history).
    #[serde(default)]
    pub canonical_head: Option<GitOid>,
    pub schema: String,
    pub active: usize,
    pub total: usize,
    pub sessions: Vec<SessionSummary>,
}

/// One action taken by `doubledoor recover`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryAction {
    pub session: DoorSessionId,
    pub action: String,
    pub detail: String,
}

/// Result of replaying journals and reconciling metadata with reality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub sessions_scanned: usize,
    pub actions: Vec<RecoveryAction>,
}

/// One check performed by `doubledoor doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Aggregate result of `doubledoor doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

/// One ghost as an actor sees it (§31/§32): what changed, who changed it,
/// why it matters, and what accepting would import versus leave held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhostView {
    pub id: GhostDiffId,
    pub session: DoorSessionId,
    /// `pending` (undecided), `deferred`, `accepted`, or `rejected`.
    pub status: String,
    /// snake_case §12 class.
    pub classification: String,
    /// Whether this ghost blocks the session until assessed.
    pub needs_assessment: bool,
    pub source_actor: Option<String>,
    pub source_commit: GitOid,
    pub parent_commit: GitOid,
    pub intent: Option<String>,
    /// Every surface the commit touched.
    pub affected: Vec<String>,
    /// Affected surfaces in any of the session's §11 sets.
    pub intersections: Vec<String>,
    /// Affected files outside the session's write set whose changes neither
    /// overwrite the actor's work (§15) nor break its code.
    pub safe_imports: Vec<String>,
    /// Affected files outside the write set whose import carries a change
    /// that breaks (or cannot be cleared for) the session's code (Phase III).
    #[serde(default)]
    pub breaking_imports: Vec<String>,
    /// Affected surfaces in the session's write set: kept as the actor's
    /// version unless explicitly accepted.
    pub held: Vec<String>,
    /// For accepted ghosts, the workspace commit carrying the import.
    pub applied_commit: Option<GitOid>,
    /// Per-surface assessment (files, Rust symbols, manifest entries).
    #[serde(default)]
    pub assessments: Vec<AssessmentView>,
    /// Assumptions of the session's work this ghost breaks (§31
    /// "ASSUMPTION INVALIDATED").
    #[serde(default)]
    pub invalidated_assumptions: Vec<AssumptionView>,
    /// Public-API symbols the ghost changed incompatibly / additively.
    #[serde(default)]
    pub api_breaking: Vec<String>,
    #[serde(default)]
    pub api_additive: Vec<String>,
    pub evidence: Vec<String>,
}

/// One surface's assessment within a ghost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessmentView {
    pub surface: String,
    /// snake_case §12 class.
    pub classification: String,
    pub reason: String,
}

/// One invalidated assumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssumptionView {
    pub surface: String,
    pub description: String,
}

/// A session's derived symbol-level footprint (`doubledoor surfaces`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfacesView {
    pub session: DoorSessionId,
    pub changed_files: Vec<String>,
    /// `rust:` ids of symbols the session added, removed, or modified,
    /// each with the kind of change.
    pub modified_symbols: Vec<AssessmentView>,
    /// Distinct names the session's changed code refers to.
    pub references: Vec<String>,
    pub external_crates: Vec<String>,
    pub crate_dirs: Vec<String>,
    pub fallback_files: Vec<String>,
    pub unparsed_files: Vec<String>,
}

/// Result of `accept` / `defer` / `reject`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionView {
    pub ghost: GhostDiffId,
    pub session: DoorSessionId,
    pub decision: String,
    /// Surfaces imported by an accept (empty otherwise).
    pub surfaces: Vec<String>,
    pub applied_commit: Option<GitOid>,
    /// Session state after the decision.
    pub state: DoorState,
    /// Ghosts that still block the session.
    pub outstanding: usize,
}

/// One session's result from an observation pass (`watch`, `ghosts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionObservation {
    pub session: DoorSessionId,
    pub new_ghosts: Vec<GhostDiffId>,
    pub cursor: GitOid,
    pub state: DoorState,
    /// Ghosts accepted automatically by the session's policy (Phase VI).
    #[serde(default)]
    pub auto_accepted: Vec<GhostDiffId>,
}
