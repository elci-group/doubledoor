//! Property test: any legal DoorState transition sequence (Phase I + Phase II
//! ghost-observation events), appended to the journal, replays back exactly
//! (§3: determinism is a core requirement).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use doubledoor_core::{DoorEvent, DoorSessionId, DoorState};
use doubledoor_journal::{Journal, JournalEventKind, JournalRecord, ReplayReport};
use proptest::prelude::*;
use proptest::strategy::Strategy;

fn door_event() -> impl Strategy<Value = DoorEvent> {
    prop::sample::select(DoorEvent::ALL.to_vec())
}

fn kind_for(event: DoorEvent, step: usize) -> JournalEventKind {
    match event {
        DoorEvent::IsolationBegun => JournalEventKind::SessionCreated,
        DoorEvent::WorkspaceReady => JournalEventKind::WorkspaceCreated {
            path: PathBuf::from(format!("/tmp/ws-{step}")),
        },
        DoorEvent::IsolationFailed => JournalEventKind::WorkspaceCreationFailed {
            reason: format!("clone failed at step {step}"),
        },
        DoorEvent::AbandonRequested => JournalEventKind::SessionAbandoned,
        DoorEvent::GhostObserved => JournalEventKind::GhostObserved,
        DoorEvent::AssessmentBegun => JournalEventKind::AssessmentBegun,
        DoorEvent::AssessmentConcluded => JournalEventKind::AssessmentConcluded,
        other => JournalEventKind::Transition {
            event: other,
            detail: format!("step {step}"),
        },
    }
}

fn temp_session_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "doubledoor-journal-proptest-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

proptest! {
    #[test]
    fn legal_transition_sequences_replay_exactly(
        events in prop::collection::vec(door_event(), 0..24)
    ) {
        // Fold the random event stream into the longest legal prefix,
        // starting from Requested and stopping at a terminal state.
        let session = DoorSessionId::new();
        let mut state = DoorState::Requested;
        let mut expected: Vec<JournalRecord> = Vec::new();
        for (step, event) in events.into_iter().enumerate() {
            let Ok(next) = state.successor(event) else {
                break;
            };
            expected.push(JournalRecord {
                session,
                at: SystemTime::UNIX_EPOCH + Duration::from_secs(step as u64),
                from: state,
                to: next,
                event: kind_for(event, step),
            });
            state = next;
            if state.is_terminal() {
                break;
            }
        }

        let dir = temp_session_dir();
        {
            let mut journal = Journal::open(&dir)?;
            for record in expected.iter().cloned() {
                journal.append(record)?;
            }
        }

        let ReplayReport { records, torn_tail } = doubledoor_journal::replay(&dir)?;
        prop_assert!(!torn_tail);
        prop_assert_eq!(records.len(), expected.len());
        for (got, want) in records.iter().zip(expected.iter()) {
            prop_assert_eq!(got.session, want.session);
            prop_assert_eq!(got.at, want.at);
            prop_assert_eq!(got.from, want.from);
            prop_assert_eq!(got.to, want.to);
            prop_assert_eq!(got.event.clone(), want.event.clone());
        }

        std::fs::remove_dir_all(&dir)?;
    }
}
