//! The DoorSession lifecycle state machine (§8, Phase I + Phase II "Ghost
//! Observation" subset).
//!
//! Phase I implements: `REQUESTED → ISOLATING → ACTIVE`, plus the terminal
//! `ABANDONED`, `FAILED_ISOLATION`, `COMPLETED`. Phase II adds the ghost
//! observation cycle: `ACTIVE → GHOST_PENDING → ASSESSING → ACTIVE`. The
//! direct `GHOST_PENDING → ACTIVE` shortcut is deliberately illegal (DD-I2:
//! no silent world-state entry — a pending ghost must pass through explicit
//! assessment). Phase IV adds reconciliation, validation, and promotion:
//! `ACTIVE → RECONCILING → VALIDATING → READY → PROMOTING → COMPLETED`, with
//! `VALIDATION_FAILED`, aborts back to `ACTIVE`, and `CanonicalAdvanced`
//! returning stale candidates to `GHOST_PENDING` (§23).
//! Every transition is explicit and total: an illegal combination is a typed
//! [`TransitionError`], never a coercion.

use serde::{Deserialize, Serialize};

use crate::error::TransitionError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DoorState {
    Requested,
    Isolating,
    Active,
    GhostPending,
    Assessing,
    /// Phase IV: a candidate is being constructed (session frozen, §16).
    Reconciling,
    /// Phase IV: the candidate is going through the validation gates (§22).
    Validating,
    /// Phase IV: the candidate passed validation and may be promoted (§23).
    Ready,
    /// Phase IV: the candidate failed a validation gate. Not terminal: the
    /// actor discards the candidate and continues (§8 FAILED).
    ValidationFailed,
    /// Phase IV: the candidate is being compare-and-swapped onto canonical.
    Promoting,
    Abandoned,
    FailedIsolation,
    Completed,
}

impl std::fmt::Display for DoorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DoorState::Requested => "REQUESTED",
            DoorState::Isolating => "ISOLATING",
            DoorState::Active => "ACTIVE",
            DoorState::GhostPending => "GHOST_PENDING",
            DoorState::Assessing => "ASSESSING",
            DoorState::Reconciling => "RECONCILING",
            DoorState::Validating => "VALIDATING",
            DoorState::Ready => "READY",
            DoorState::ValidationFailed => "VALIDATION_FAILED",
            DoorState::Promoting => "PROMOTING",
            DoorState::Abandoned => "ABANDONED",
            DoorState::FailedIsolation => "FAILED_ISOLATION",
            DoorState::Completed => "COMPLETED",
        };
        f.write_str(s)
    }
}

/// Events that drive state transitions. Named for journal fidelity (§25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DoorEvent {
    IsolationBegun,
    WorkspaceReady,
    IsolationFailed,
    AbandonRequested,
    /// The candidate became canonical and the session is finished.
    /// PROMOTING → COMPLETED.
    PromotionCompleted,
    /// Reconciliation began. ACTIVE → RECONCILING.
    ReconcileBegun,
    /// A candidate commit was constructed. RECONCILING → VALIDATING.
    CandidateBuilt,
    /// Reconciliation or validation stopped without a result (conflict that
    /// prevents construction, cancellation, crash); the candidate is marked
    /// incomplete (§43). RECONCILING | VALIDATING → ACTIVE.
    ReconcileAborted,
    /// Every required validation gate passed. VALIDATING → READY.
    GatesPassed,
    /// A required validation gate failed. VALIDATING → VALIDATION_FAILED.
    GatesFailed,
    /// The actor set the candidate aside. READY | VALIDATION_FAILED → ACTIVE.
    CandidateDiscarded,
    /// Promotion began. READY → PROMOTING.
    PromotionBegun,
    /// Only the candidate's converged portion became canonical; contested
    /// work stays isolated in the session. PROMOTING → ACTIVE.
    PartialPromotionCompleted,
    /// Promotion stopped before canonical changed. PROMOTING → READY.
    PromotionAborted,
    /// Canonical advanced past the candidate's target (§23): the candidate is
    /// stale and discarded, and the session returns to assessment — the new
    /// canonical commits are observed next, and any that need assessment
    /// move it on to GHOST_PENDING. VALIDATING | READY | PROMOTING → ACTIVE.
    CanonicalAdvanced,
    /// Re-run validation of the current candidate.
    /// READY | VALIDATION_FAILED → VALIDATING.
    ValidationBegun,
    /// A ghost was generated for the session: external commits exist that the
    /// actor has not yet assessed. ACTIVE → GHOST_PENDING.
    GhostObserved,
    /// The actor begins assessing the pending ghost. GHOST_PENDING → ASSESSING.
    AssessmentBegun,
    /// The actor recorded an assessment decision (accept applied / defer /
    /// reject). ASSESSING → ACTIVE.
    AssessmentConcluded,
}

impl std::fmt::Display for DoorEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DoorEvent::IsolationBegun => "IsolationBegun",
            DoorEvent::WorkspaceReady => "WorkspaceReady",
            DoorEvent::IsolationFailed => "IsolationFailed",
            DoorEvent::AbandonRequested => "AbandonRequested",
            DoorEvent::PromotionCompleted => "PromotionCompleted",
            DoorEvent::GhostObserved => "GhostObserved",
            DoorEvent::AssessmentBegun => "AssessmentBegun",
            DoorEvent::AssessmentConcluded => "AssessmentConcluded",
            DoorEvent::ReconcileBegun => "ReconcileBegun",
            DoorEvent::CandidateBuilt => "CandidateBuilt",
            DoorEvent::ReconcileAborted => "ReconcileAborted",
            DoorEvent::GatesPassed => "GatesPassed",
            DoorEvent::GatesFailed => "GatesFailed",
            DoorEvent::CandidateDiscarded => "CandidateDiscarded",
            DoorEvent::PromotionBegun => "PromotionBegun",
            DoorEvent::PartialPromotionCompleted => "PartialPromotionCompleted",
            DoorEvent::PromotionAborted => "PromotionAborted",
            DoorEvent::CanonicalAdvanced => "CanonicalAdvanced",
            DoorEvent::ValidationBegun => "ValidationBegun",
        };
        f.write_str(s)
    }
}

impl DoorEvent {
    /// Every event, for exhaustive property and table tests.
    pub const ALL: [DoorEvent; 19] = [
        DoorEvent::IsolationBegun,
        DoorEvent::WorkspaceReady,
        DoorEvent::IsolationFailed,
        DoorEvent::AbandonRequested,
        DoorEvent::PromotionCompleted,
        DoorEvent::GhostObserved,
        DoorEvent::AssessmentBegun,
        DoorEvent::AssessmentConcluded,
        DoorEvent::ReconcileBegun,
        DoorEvent::CandidateBuilt,
        DoorEvent::ReconcileAborted,
        DoorEvent::GatesPassed,
        DoorEvent::GatesFailed,
        DoorEvent::CandidateDiscarded,
        DoorEvent::PromotionBegun,
        DoorEvent::PartialPromotionCompleted,
        DoorEvent::PromotionAborted,
        DoorEvent::CanonicalAdvanced,
        DoorEvent::ValidationBegun,
    ];
}

impl DoorState {
    /// The single legal successor for (`self`, `event`), if any.
    pub fn successor(self, event: DoorEvent) -> Result<DoorState, TransitionError> {
        use DoorEvent::*;
        use DoorState::*;
        let next = match (self, event) {
            (Requested, IsolationBegun) => Isolating,
            (Isolating, WorkspaceReady) => Active,
            (Isolating, IsolationFailed) => FailedIsolation,
            // Abandon from anywhere non-terminal except mid-promotion, which
            // must resolve (to COMPLETED, READY, or GHOST_PENDING) first.
            (
                Active | Isolating | Requested | GhostPending | Assessing | Reconciling
                | Validating | Ready | ValidationFailed,
                AbandonRequested,
            ) => Abandoned,
            (Active, GhostObserved) => GhostPending,
            (GhostPending, AssessmentBegun) => Assessing,
            (Assessing, AssessmentConcluded) => Active,
            (Active, ReconcileBegun) => Reconciling,
            (Reconciling, CandidateBuilt) => Validating,
            (Reconciling | Validating, ReconcileAborted) => Active,
            (Validating, GatesPassed) => Ready,
            (Validating, GatesFailed) => ValidationFailed,
            (Ready | ValidationFailed, CandidateDiscarded) => Active,
            (Ready, PromotionBegun) => Promoting,
            (Promoting, PromotionCompleted) => Completed,
            (Promoting, PartialPromotionCompleted) => Active,
            (Promoting, PromotionAborted) => Ready,
            (Validating | Ready | Promoting, CanonicalAdvanced) => Active,
            (Ready | ValidationFailed, ValidationBegun) => Validating,
            _ => {
                return Err(TransitionError { from: self, event });
            }
        };
        Ok(next)
    }

    /// States from which a session counts against `max_active_sessions`:
    /// every non-terminal state (they all hold, or are about to hold, a
    /// workspace).
    pub fn counts_as_active(self) -> bool {
        !self.is_terminal()
    }

    /// States in which the session holds a workspace.
    pub fn holds_workspace(self) -> bool {
        matches!(
            self,
            DoorState::Active
                | DoorState::GhostPending
                | DoorState::Assessing
                | DoorState::Reconciling
                | DoorState::Validating
                | DoorState::Ready
                | DoorState::ValidationFailed
                | DoorState::Promoting
        )
    }

    /// Terminal states — no further transitions are legal.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DoorState::Abandoned | DoorState::FailedIsolation | DoorState::Completed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(from: DoorState, event: DoorEvent, expect: DoorState) {
        assert_eq!(from.successor(event).unwrap(), expect);
    }

    fn bad(from: DoorState, event: DoorEvent) {
        let err = from.successor(event).unwrap_err();
        assert_eq!(err.from, from);
        assert_eq!(err.event, event);
    }

    #[test]
    fn happy_path() {
        ok(
            DoorState::Requested,
            DoorEvent::IsolationBegun,
            DoorState::Isolating,
        );
        ok(
            DoorState::Isolating,
            DoorEvent::WorkspaceReady,
            DoorState::Active,
        );
        ok(
            DoorState::Active,
            DoorEvent::ReconcileBegun,
            DoorState::Reconciling,
        );
        ok(
            DoorState::Reconciling,
            DoorEvent::CandidateBuilt,
            DoorState::Validating,
        );
        ok(
            DoorState::Validating,
            DoorEvent::GatesPassed,
            DoorState::Ready,
        );
        ok(
            DoorState::Ready,
            DoorEvent::PromotionBegun,
            DoorState::Promoting,
        );
        ok(
            DoorState::Promoting,
            DoorEvent::PromotionCompleted,
            DoorState::Completed,
        );
    }

    #[test]
    fn promotion_requires_a_validated_candidate() {
        // No shortcut from ACTIVE to COMPLETED any more (§23).
        bad(DoorState::Active, DoorEvent::PromotionCompleted);
        bad(DoorState::Active, DoorEvent::PromotionBegun);
        bad(DoorState::Validating, DoorEvent::PromotionBegun);
        bad(DoorState::ValidationFailed, DoorEvent::PromotionBegun);
        bad(DoorState::Reconciling, DoorEvent::GatesPassed);
    }

    #[test]
    fn stale_candidates_return_to_assessment() {
        // Back to ACTIVE; observation then decides whether the new commits
        // need assessment (GHOST_PENDING) — never stuck pending nothing.
        for s in [
            DoorState::Validating,
            DoorState::Ready,
            DoorState::Promoting,
        ] {
            ok(s, DoorEvent::CanonicalAdvanced, DoorState::Active);
        }
        bad(DoorState::Active, DoorEvent::CanonicalAdvanced);
        ok(
            DoorState::Ready,
            DoorEvent::ValidationBegun,
            DoorState::Validating,
        );
        ok(
            DoorState::ValidationFailed,
            DoorEvent::ValidationBegun,
            DoorState::Validating,
        );
        bad(DoorState::Active, DoorEvent::ValidationBegun);
    }

    #[test]
    fn failures_and_aborts_resume_speculation() {
        ok(
            DoorState::Validating,
            DoorEvent::GatesFailed,
            DoorState::ValidationFailed,
        );
        assert!(!DoorState::ValidationFailed.is_terminal());
        ok(
            DoorState::ValidationFailed,
            DoorEvent::CandidateDiscarded,
            DoorState::Active,
        );
        ok(
            DoorState::Ready,
            DoorEvent::CandidateDiscarded,
            DoorState::Active,
        );
        ok(
            DoorState::Reconciling,
            DoorEvent::ReconcileAborted,
            DoorState::Active,
        );
        ok(
            DoorState::Validating,
            DoorEvent::ReconcileAborted,
            DoorState::Active,
        );
        ok(
            DoorState::Promoting,
            DoorEvent::PromotionAborted,
            DoorState::Ready,
        );
        ok(
            DoorState::Promoting,
            DoorEvent::PartialPromotionCompleted,
            DoorState::Active,
        );
        // Mid-promotion must resolve before abandonment.
        bad(DoorState::Promoting, DoorEvent::AbandonRequested);
    }

    #[test]
    fn isolation_failure_is_terminal_typed_error_source() {
        ok(
            DoorState::Isolating,
            DoorEvent::IsolationFailed,
            DoorState::FailedIsolation,
        );
        bad(DoorState::FailedIsolation, DoorEvent::WorkspaceReady);
        bad(DoorState::FailedIsolation, DoorEvent::AbandonRequested);
    }

    #[test]
    fn abandon_is_legal_from_non_terminal_states() {
        ok(
            DoorState::Requested,
            DoorEvent::AbandonRequested,
            DoorState::Abandoned,
        );
        ok(
            DoorState::Isolating,
            DoorEvent::AbandonRequested,
            DoorState::Abandoned,
        );
        ok(
            DoorState::Active,
            DoorEvent::AbandonRequested,
            DoorState::Abandoned,
        );
        ok(
            DoorState::GhostPending,
            DoorEvent::AbandonRequested,
            DoorState::Abandoned,
        );
        ok(
            DoorState::Assessing,
            DoorEvent::AbandonRequested,
            DoorState::Abandoned,
        );
        bad(DoorState::Completed, DoorEvent::AbandonRequested);
        bad(DoorState::Abandoned, DoorEvent::AbandonRequested);
    }

    #[test]
    fn ghost_observation_cycle() {
        ok(
            DoorState::Active,
            DoorEvent::GhostObserved,
            DoorState::GhostPending,
        );
        ok(
            DoorState::GhostPending,
            DoorEvent::AssessmentBegun,
            DoorState::Assessing,
        );
        ok(
            DoorState::Assessing,
            DoorEvent::AssessmentConcluded,
            DoorState::Active,
        );
    }

    #[test]
    fn ghost_pending_cannot_be_cleared_without_assessment() {
        // DD-I2: no silent world-state entry. A pending ghost must pass
        // through explicit assessment before the session is ACTIVE again.
        bad(DoorState::GhostPending, DoorEvent::AssessmentConcluded);
        bad(DoorState::GhostPending, DoorEvent::WorkspaceReady);
        bad(DoorState::GhostPending, DoorEvent::GhostObserved);
        bad(DoorState::GhostPending, DoorEvent::PromotionCompleted);
    }

    #[test]
    fn assessment_requires_a_pending_ghost() {
        bad(DoorState::Active, DoorEvent::AssessmentBegun);
        bad(DoorState::Active, DoorEvent::AssessmentConcluded);
        bad(DoorState::Isolating, DoorEvent::GhostObserved);
        bad(DoorState::Requested, DoorEvent::GhostObserved);
    }

    #[test]
    fn no_skipping_isolation() {
        bad(DoorState::Requested, DoorEvent::WorkspaceReady);
        bad(DoorState::Requested, DoorEvent::PromotionCompleted);
        bad(DoorState::Isolating, DoorEvent::PromotionCompleted);
    }

    #[test]
    fn terminal_states_have_no_successors() {
        for state in [
            DoorState::Abandoned,
            DoorState::FailedIsolation,
            DoorState::Completed,
        ] {
            assert!(state.is_terminal());
            for event in DoorEvent::ALL {
                assert!(state.successor(event).is_err(), "{state:?} {event:?}");
            }
        }
    }

    #[test]
    fn ghost_states_are_not_terminal() {
        assert!(!DoorState::GhostPending.is_terminal());
        assert!(!DoorState::Assessing.is_terminal());
    }

    #[test]
    fn activity_accounting() {
        assert!(DoorState::Requested.counts_as_active());
        assert!(DoorState::Isolating.counts_as_active());
        assert!(DoorState::Active.counts_as_active());
        assert!(DoorState::GhostPending.counts_as_active());
        assert!(DoorState::Assessing.counts_as_active());
        assert!(!DoorState::Abandoned.counts_as_active());
        assert!(!DoorState::FailedIsolation.counts_as_active());
        assert!(!DoorState::Completed.counts_as_active());
        assert!(DoorState::Ready.counts_as_active());
        assert!(DoorState::Promoting.holds_workspace());
        assert!(!DoorState::Isolating.holds_workspace());
    }
}
