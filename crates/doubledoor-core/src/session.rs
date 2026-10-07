//! The DoorSession aggregate (§7, Phase I/II subset) and the policy snapshot
//! carried with it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::error::TransitionError;
use crate::id::{ActorId, DoorSessionId, GitOid, SurfaceId};
use crate::notice::IntentDescriptor;
use crate::state::{DoorEvent, DoorState};
use crate::surface::SurfaceSet;

/// The authoritative repository state from which a DoorSession originates
/// (§5.1). Immutable for the life of the session (DD-I3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalState {
    pub commit: GitOid,
    pub tree: GitOid,
    pub captured_at: SystemTime,
}

/// Reference to an isolated speculative workspace on disk (§9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRef {
    pub path: PathBuf,
}

/// The policy configuration in force when the session was created (§7).
/// Phase I carries only resource quotas (§28); ghost/reconcile rules slot
/// in here in later phases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub max_active_sessions: u32,
    pub max_workspace_bytes: u64,
    pub max_session_lifetime_secs: u64,
    /// §28: candidates a session may build (Phase IV).
    #[serde(default = "default_max_reconciliation_attempts")]
    pub max_reconciliation_attempts: u32,
    /// §28: wall-clock budget for one validation run (Phase IV).
    #[serde(default = "default_max_validation_runtime_secs")]
    pub max_validation_runtime_secs: u64,
}

fn default_max_reconciliation_attempts() -> u32 {
    20
}

fn default_max_validation_runtime_secs() -> u64 {
    30 * 60
}

impl Default for PolicySnapshot {
    fn default() -> Self {
        PolicySnapshot {
            max_active_sessions: 100,
            max_workspace_bytes: 10 * 1024 * 1024 * 1024,
            max_session_lifetime_secs: 7 * 24 * 3600,
            max_reconciliation_attempts: default_max_reconciliation_attempts(),
            max_validation_runtime_secs: default_max_validation_runtime_secs(),
        }
    }
}

/// How far ghost observation has advanced for a session (§46): every commit
/// up to and including `commit` has been folded into generated GhostDiffs.
/// At creation this is the session's base commit; the CLI advances it after a
/// successful observation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhostCursor {
    pub commit: GitOid,
}

impl GhostCursor {
    pub fn new(commit: GitOid) -> Self {
        GhostCursor { commit }
    }

    /// The null (all-zeros) commit: the cursor of a session whose metadata
    /// predates Phase II and therefore never recorded one. An observation run
    /// treats the session's base commit as the effective cursor in that case.
    pub fn unborn() -> Self {
        GhostCursor {
            commit: GitOid::parse(&"0".repeat(40)).expect("40 hex digits"),
        }
    }

    /// `true` when this cursor predates Phase II metadata (`unborn`).
    pub fn is_unborn(&self) -> bool {
        self.commit == Self::unborn().commit
    }
}

impl Default for GhostCursor {
    /// The unborn cursor, used as the serde fallback so metadata written
    /// before Phase II (which carried no cursor field) still deserializes.
    fn default() -> Self {
        Self::unborn()
    }
}

/// Automatic low-risk acceptance (§50): which ghosts may enter the session's
/// workspace without a per-ghost decision. An explicit, journalled opt-in by
/// the actor (DD-I2); `Off` by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoAcceptLevel {
    /// Every ghost waits for the actor.
    #[default]
    Off,
    /// Ghosts with no relationship to the session at any layer: every
    /// assessment independent, no invalidated assumption.
    Independent,
    /// Additionally ghosts the session merely depends on, provided nothing
    /// they change is API-breaking and no assumption is invalidated.
    DependencyRelevant,
}

impl AutoAcceptLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(AutoAcceptLevel::Off),
            "independent" => Some(AutoAcceptLevel::Independent),
            "dependency_relevant" | "dependency-relevant" => {
                Some(AutoAcceptLevel::DependencyRelevant)
            }
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AutoAcceptLevel::Off => "off",
            AutoAcceptLevel::Independent => "independent",
            AutoAcceptLevel::DependencyRelevant => "dependency_relevant",
        }
    }
}

/// An isolated speculative execution context (§5.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoorSession {
    pub id: DoorSessionId,
    pub actor: ActorId,
    pub intent: IntentDescriptor,

    /// Immutable base identity (DD-I3).
    pub base: CanonicalState,

    /// Present once isolation has produced a workspace.
    pub workspace: Option<WorkspaceRef>,

    pub requested_surfaces: Vec<SurfaceId>,
    pub permitted_surfaces: Vec<SurfaceId>,

    /// Surfaces the actor reads (§11). Empty at Phase II session creation;
    /// the CLI populates the sets when the actor's plan is registered.
    #[serde(default)]
    pub read_set: SurfaceSet,

    /// Surfaces the actor intends to mutate (§11).
    #[serde(default)]
    pub write_set: SurfaceSet,

    /// Surfaces the actor's work structurally depends on without reading or
    /// writing them directly (§11).
    #[serde(default)]
    pub dependency_set: SurfaceSet,

    /// How far ghost observation has advanced (§46). Defaults to the base
    /// commit at creation; `#[serde(default)]` keeps pre-Phase-II metadata
    /// loadable (the cursor then deserializes as [`GhostCursor::unborn`]).
    #[serde(default)]
    pub ghost_cursor: GhostCursor,

    pub policy: PolicySnapshot,

    /// Automatic low-risk acceptance (Phase VI). Defaulted so earlier
    /// metadata loads with it off.
    #[serde(default)]
    pub auto_accept: AutoAcceptLevel,

    pub state: DoorState,
    pub created_at: SystemTime,
}

impl DoorSession {
    /// Construct a session in `Requested` state.
    pub fn new(
        actor: ActorId,
        intent: IntentDescriptor,
        base: CanonicalState,
        requested_surfaces: Vec<SurfaceId>,
        policy: PolicySnapshot,
    ) -> Self {
        DoorSession {
            id: DoorSessionId::new(),
            actor,
            intent,
            base: base.clone(),
            workspace: None,
            requested_surfaces,
            permitted_surfaces: Vec::new(),
            read_set: SurfaceSet::new(),
            write_set: SurfaceSet::new(),
            dependency_set: SurfaceSet::new(),
            ghost_cursor: GhostCursor::new(base.commit),
            policy,
            auto_accept: AutoAcceptLevel::Off,
            state: DoorState::Requested,
            created_at: SystemTime::now(),
        }
    }

    /// Apply an event, mutating state only on success. Illegal transitions
    /// leave the session untouched and return a typed error (§8).
    pub fn transition(&mut self, event: DoorEvent) -> Result<DoorState, TransitionError> {
        let next = self.state.successor(event)?;
        self.state = next;
        Ok(next)
    }

    /// Record the workspace produced by isolation and mark the session
    /// active. Only legal from `Isolating`.
    pub fn activate_with(&mut self, workspace: WorkspaceRef) -> Result<DoorState, TransitionError> {
        let next = self.transition(DoorEvent::WorkspaceReady)?;
        self.workspace = Some(workspace);
        Ok(next)
    }

    /// Seconds since session creation.
    pub fn age_secs(&self, now: SystemTime) -> u64 {
        now.duration_since(self.created_at)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> CanonicalState {
        CanonicalState {
            commit: GitOid::parse(&"a".repeat(40)).unwrap(),
            tree: GitOid::parse(&"b".repeat(40)).unwrap(),
            captured_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn session() -> DoorSession {
        DoorSession::new(
            ActorId::new("codex-7").unwrap(),
            IntentDescriptor::new("implement retry policy"),
            base(),
            vec![SurfaceId::new("crate::router").unwrap()],
            PolicySnapshot::default(),
        )
    }

    #[test]
    fn new_session_is_requested_with_stable_base() {
        let s = session();
        assert_eq!(s.state, DoorState::Requested);
        assert_eq!(s.base, base());
        assert!(s.workspace.is_none());
    }

    #[test]
    fn illegal_transition_leaves_session_unchanged() {
        let mut s = session();
        let before = s.clone();
        assert!(s.transition(DoorEvent::WorkspaceReady).is_err());
        assert_eq!(s.state, DoorState::Requested);
        assert_eq!(s, before, "failed transition must not mutate the session");
    }

    #[test]
    fn activate_attaches_workspace_exactly_once() {
        let mut s = session();
        s.transition(DoorEvent::IsolationBegun).unwrap();
        let ws = WorkspaceRef {
            path: PathBuf::from("/tmp/ws"),
        };
        s.activate_with(ws.clone()).unwrap();
        assert_eq!(s.state, DoorState::Active);
        assert_eq!(s.workspace, Some(ws));
        // A second activation is not a legal transition.
        assert!(s
            .activate_with(WorkspaceRef {
                path: PathBuf::from("/tmp/other"),
            })
            .is_err());
    }

    #[test]
    fn base_is_immutable_across_the_lifecycle() {
        let mut s = session();
        let base = s.base.clone();
        s.transition(DoorEvent::IsolationBegun).unwrap();
        s.transition(DoorEvent::AbandonRequested).unwrap();
        assert_eq!(s.base, base, "base identity must never change (DD-I3)");
    }

    #[test]
    fn new_session_starts_with_base_cursor_and_empty_sets() {
        let s = session();
        assert_eq!(
            s.ghost_cursor,
            GhostCursor::new(s.base.commit.clone()),
            "cursor defaults to the base commit (§46)"
        );
        assert!(s.read_set.is_empty());
        assert!(s.write_set.is_empty());
        assert!(s.dependency_set.is_empty());
    }

    #[test]
    fn ghost_observation_drives_session_through_assessment() {
        let mut s = session();
        s.transition(DoorEvent::IsolationBegun).unwrap();
        s.activate_with(WorkspaceRef {
            path: PathBuf::from("/tmp/ws"),
        })
        .unwrap();

        s.transition(DoorEvent::GhostObserved).unwrap();
        assert_eq!(s.state, DoorState::GhostPending);
        // No silent world-state entry: the session cannot skip assessment.
        assert!(s.transition(DoorEvent::GhostObserved).is_err());
        assert!(s.transition(DoorEvent::WorkspaceReady).is_err());

        s.transition(DoorEvent::AssessmentBegun).unwrap();
        assert_eq!(s.state, DoorState::Assessing);
        s.transition(DoorEvent::AssessmentConcluded).unwrap();
        assert_eq!(s.state, DoorState::Active);
    }

    #[test]
    fn pre_phase_ii_metadata_loads_with_defaulted_fields() {
        let s = session();
        let mut value = serde_json::to_value(&s).unwrap();
        // Metadata written before Phase II carries none of the new fields.
        for key in ["read_set", "write_set", "dependency_set", "ghost_cursor"] {
            value.as_object_mut().unwrap().remove(key);
        }
        let loaded: DoorSession = serde_json::from_value(value).unwrap();
        assert!(loaded.read_set.is_empty());
        assert!(loaded.write_set.is_empty());
        assert!(loaded.dependency_set.is_empty());
        assert!(
            loaded.ghost_cursor.is_unborn(),
            "missing cursor must fall back to the unborn sentinel, not fail"
        );
    }
}
