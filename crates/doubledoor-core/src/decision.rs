//! Ghost assessment decisions (§14, Phase II).
//!
//! An actor answers a pending ghost with one of the principal responses.
//! Phase II implements three of the four: ACCEPT (optionally
//! surface-selective, §15), DEFER, and REJECT. RECONCILE belongs to the
//! reconciliation engine (Phase IV). A decision governs one DoorSession only:
//! rejecting a ghost never reverts canonical history (§14).

use serde::{Deserialize, Serialize};

use crate::id::{GitOid, SurfaceId};

/// The actor's recorded answer to one ghost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GhostDecision {
    /// The ghost's changes to `surfaces` entered the speculative world-state.
    /// `applied_commit` is the workspace commit that carries them, or `None`
    /// when the selected changes were already present (nothing to commit).
    Accepted {
        surfaces: Vec<SurfaceId>,
        applied_commit: Option<GitOid>,
    },
    /// Acknowledged; the actor continues against its current state. The ghost
    /// stays pending and can still be accepted or rejected later.
    Deferred,
    /// Declared unsuitable for this session. Canonical history is untouched.
    Rejected,
}

impl GhostDecision {
    /// `true` for a decision that closes the ghost for good (accept/reject).
    /// A deferred ghost remains open.
    pub fn is_final(&self) -> bool {
        !matches!(self, GhostDecision::Deferred)
    }

    /// Stable lower-case name used in CLI output and the event stream.
    pub fn name(&self) -> &'static str {
        match self {
            GhostDecision::Accepted { .. } => "accepted",
            GhostDecision::Deferred => "deferred",
            GhostDecision::Rejected => "rejected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_defer_leaves_a_ghost_open() {
        assert!(!GhostDecision::Deferred.is_final());
        assert!(GhostDecision::Rejected.is_final());
        assert!(GhostDecision::Accepted {
            surfaces: Vec::new(),
            applied_commit: None,
        }
        .is_final());
    }

    #[test]
    fn decision_roundtrips_through_serialization() {
        let d = GhostDecision::Accepted {
            surfaces: vec![SurfaceId::new("file:src/retry.rs").unwrap()],
            applied_commit: Some(GitOid::parse(&"c".repeat(40)).unwrap()),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<GhostDecision>(&json).unwrap(), d);
    }
}
