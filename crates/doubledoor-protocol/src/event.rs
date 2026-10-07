//! The local event stream (§13, Phase II): what agents subscribe to instead
//! of polling Git.
//!
//! Events are transport-neutral values. Phase II delivers them through an
//! append-only JSONL log at `.doubledoor/events.jsonl` (see
//! `doubledoor-journal`'s `EventLog`) that `doubledoor events --follow`
//! streams; a socket, HTTP, or WebSocket transport can carry the same values
//! later without changing their shape.

use serde::{Deserialize, Serialize};

use doubledoor_core::{CandidateId, DoorSessionId, DoorState, GhostDiffId, GitOid};

/// One event on the stream, tagged by its §13 dotted name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum StreamEvent {
    /// A canonical commit was observed for a session and recorded as a ghost.
    #[serde(rename = "ghost.available")]
    GhostAvailable {
        session: DoorSessionId,
        ghost: GhostDiffId,
        source_commit: GitOid,
        source_actor: Option<String>,
        /// snake_case §12 class, e.g. `write_overlap`.
        classification: String,
        /// The session surfaces the commit touched.
        intersections: Vec<String>,
        /// `false` only for `independent` ghosts; `unknown` always needs
        /// assessment (§12: unknown is not safe).
        needs_assessment: bool,
    },
    /// The actor answered a ghost with accept/defer/reject.
    #[serde(rename = "ghost.assessed")]
    GhostAssessed {
        session: DoorSessionId,
        ghost: GhostDiffId,
        decision: String,
        /// `None` when the actor decided; `policy:<level>` for an automatic
        /// low-risk acceptance (Phase VI).
        #[serde(default)]
        by: Option<String>,
    },
    /// A reconciliation candidate was built (Phase IV).
    #[serde(rename = "candidate.built")]
    CandidateBuilt {
        session: DoorSessionId,
        candidate: CandidateId,
        commit: GitOid,
        promotable: bool,
        contested: Vec<String>,
    },
    /// A candidate's validation finished (Phase IV).
    #[serde(rename = "candidate.validated")]
    CandidateValidated {
        session: DoorSessionId,
        candidate: CandidateId,
        passed: bool,
    },
    /// A candidate became canonical (Phase IV). Other sessions will observe
    /// it as a ghost.
    #[serde(rename = "candidate.promoted")]
    CandidatePromoted {
        session: DoorSessionId,
        candidate: CandidateId,
        from: GitOid,
        to: GitOid,
        partial: bool,
    },
    /// A session changed lifecycle state as a result of ghost activity.
    #[serde(rename = "session.state")]
    SessionState {
        session: DoorSessionId,
        from: DoorState,
        to: DoorState,
    },
}

impl StreamEvent {
    /// The session the event concerns (every Phase II event has one).
    pub fn session(&self) -> DoorSessionId {
        match self {
            StreamEvent::GhostAvailable { session, .. }
            | StreamEvent::GhostAssessed { session, .. }
            | StreamEvent::CandidateBuilt { session, .. }
            | StreamEvent::CandidateValidated { session, .. }
            | StreamEvent::CandidatePromoted { session, .. }
            | StreamEvent::SessionState { session, .. } => *session,
        }
    }
}

/// A stream event with its position in the log. `seq` is 1-based and dense:
/// the n-th complete line of the log has `seq == n`, so a consumer resumes
/// with `--since <last seq seen>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequencedEvent {
    pub seq: u64,
    pub at_millis: u64,
    #[serde(flatten)]
    pub event: StreamEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghost_available_matches_the_section_13_shape() {
        let ev = StreamEvent::GhostAvailable {
            session: DoorSessionId::new(),
            ghost: GhostDiffId::new(),
            source_commit: GitOid::parse(&"a".repeat(40)).unwrap(),
            source_actor: Some("agent-alpha".to_string()),
            classification: "dependency_relevant".to_string(),
            intersections: vec!["file:src/router.rs".to_string()],
            needs_assessment: true,
        };
        let v = serde_json::to_value(SequencedEvent {
            seq: 7,
            at_millis: 1,
            event: ev.clone(),
        })
        .unwrap();
        assert_eq!(v["event"], "ghost.available");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["classification"], "dependency_relevant");
        assert_eq!(v["intersections"][0], "file:src/router.rs");
        let back: SequencedEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back.event, ev);
    }
}
