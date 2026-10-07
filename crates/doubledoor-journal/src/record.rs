//! Journal record types (§25, Phase I/II subset).

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use doubledoor_core::{
    AutoAcceptLevel, CandidateId, DoorEvent, DoorSessionId, DoorState, GhostDecision, GhostDiffId,
    GitOid,
};

/// What happened, in journal vocabulary (§25). The structural envelope
/// (session, timestamp, prior/new state) lives on [`JournalRecord`] so every
/// variant stays a simple payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalEventKind {
    SessionCreated,
    WorkspaceCreated {
        path: PathBuf,
    },
    WorkspaceCreationFailed {
        reason: String,
    },
    SessionActivated,
    SessionAbandoned,
    RecoveryPerformed {
        action: String,
    },
    /// A ghost was generated for the session (Phase II, §46).
    GhostObserved,
    /// The actor began assessing a pending ghost (Phase II, §46).
    AssessmentBegun,
    /// The actor recorded a ghost assessment decision (Phase II, §46).
    AssessmentConcluded,
    /// A persisted ghost record was entered into the session's evidence
    /// (Phase II, §46). No state change: `from == to`.
    GhostRecorded {
        ghost: GhostDiffId,
        source_commit: GitOid,
    },
    /// Every canonical commit up to `to` has been observed (Phase II, §46).
    GhostCursorAdvanced {
        to: GitOid,
    },
    /// The actor answered a ghost (§14). An accepted ghost's changes entered
    /// the workspace through `decision`'s `applied_commit`.
    GhostAssessed {
        ghost: GhostDiffId,
        decision: GhostDecision,
        /// Who decided: `None` for the actor, `policy:<level>` for an
        /// automatic acceptance (Phase VI).
        #[serde(default)]
        by: Option<String>,
    },
    /// The session's automatic-acceptance policy changed (Phase VI).
    PolicyChanged {
        auto_accept: AutoAcceptLevel,
    },
    /// A Phase IV lifecycle transition (reconcile, validate, promote,
    /// discard, abort, canonical advance), with a human-readable detail.
    Transition {
        event: DoorEvent,
        detail: String,
    },
    /// A reconciliation candidate was persisted (Phase IV, §16).
    CandidateRecorded {
        candidate: CandidateId,
        commit: GitOid,
        target: GitOid,
        promotable: bool,
    },
    /// A candidate's validation report was persisted (Phase IV, §22).
    ValidationRecorded {
        candidate: CandidateId,
        passed: bool,
    },
    /// A candidate's unified evidence bundle was persisted (Phase V, §49).
    EvidenceRecorded {
        candidate: CandidateId,
        verdict: String,
    },
    /// Canonical moved from `from` to `to` by promoting `candidate` (§23).
    CanonicalPromoted {
        candidate: CandidateId,
        from: GitOid,
        to: GitOid,
        partial: bool,
    },
}

/// One journal line: the event plus the session it belongs to, when it
/// happened, and the DoorState transition it effected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub session: DoorSessionId,
    pub at: SystemTime,
    pub from: DoorState,
    pub to: DoorState,
    pub event: JournalEventKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_serialize_as_internally_tagged_kinds() {
        let json = serde_json::to_string(&JournalEventKind::WorkspaceCreated {
            path: PathBuf::from("/tmp/ws"),
        })
        .unwrap();
        assert!(json.contains("WorkspaceCreated"));

        let json = serde_json::to_string(&JournalEventKind::RecoveryPerformed {
            action: "replayed 3 records".to_string(),
        })
        .unwrap();
        let back: JournalEventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back,
            JournalEventKind::RecoveryPerformed {
                action: "replayed 3 records".to_string()
            }
        );
    }
}
