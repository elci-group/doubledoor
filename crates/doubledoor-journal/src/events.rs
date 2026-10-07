//! The repository-wide event log backing the §13 local event stream.
//!
//! `.doubledoor/events.jsonl` is append-only: one
//! [`Envelope`]`<LoggedEvent>` JSON line per [`StreamEvent`]. Each append is
//! a single `write` of the complete line on an `O_APPEND` handle followed by
//! `sync_data`, so concurrent writers (a `watch` loop and an `accept` from
//! another terminal) never interleave partial lines. Sequence numbers are
//! implicit — the n-th complete line is event `n` — so no writer needs to
//! coordinate a counter.
//!
//! A reader treats a trailing line without its `\n` as not yet written: it is
//! neither returned nor counted, and is picked up once the writer finishes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use doubledoor_protocol::{Envelope, SequencedEvent, StreamEvent};

use crate::error::JournalError;

const EVENT_LOG_FILE: &str = "events.jsonl";

#[derive(Debug, Serialize, Deserialize)]
struct LoggedEvent {
    at_millis: u64,
    #[serde(flatten)]
    event: StreamEvent,
}

/// Handle to the event log under a store root (typically `.doubledoor`).
#[derive(Debug, Clone)]
pub struct EventLog {
    path: PathBuf,
}

/// Where a reader is in the log: the byte offset of the next unread line and
/// the sequence number of the last event returned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventCursor {
    offset: u64,
    seq: u64,
}

impl EventCursor {
    /// Sequence number of the last event this cursor has passed.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl EventLog {
    /// The log at `store_root/events.jsonl`. No I/O until first use.
    pub fn new(store_root: &Path) -> Self {
        EventLog {
            path: store_root.join(EVENT_LOG_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event durably (single write, then `sync_data`).
    pub fn append(&self, event: StreamEvent) -> Result<(), JournalError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let at_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut line = serde_json::to_vec(&Envelope::new(LoggedEvent { at_millis, event }))?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }

    /// Read every complete event after `cursor`, advancing it. Returns an
    /// empty vector when nothing new has been written (or the log does not
    /// exist yet). A corrupt complete line is a hard error; schema versions
    /// are checked per line (§44).
    pub fn read_new(&self, cursor: &mut EventCursor) -> Result<Vec<SequencedEvent>, JournalError> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        file.seek(SeekFrom::Start(cursor.offset))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        let mut events = Vec::new();
        let mut consumed = 0usize;
        while let Some(nl) = buf[consumed..].iter().position(|b| *b == b'\n') {
            let line = &buf[consumed..consumed + nl];
            consumed += nl + 1;
            cursor.seq += 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let env: Envelope<LoggedEvent> =
                serde_json::from_slice(line).map_err(|e| JournalError::Corrupt {
                    line: cursor.seq as usize,
                    reason: format!("{} ({e})", self.path.display()),
                })?;
            env.check_schema()
                .map_err(|e| JournalError::UnsupportedSchema(e.to_string()))?;
            events.push(SequencedEvent {
                seq: cursor.seq,
                at_millis: env.payload.at_millis,
                event: env.payload.event,
            });
        }
        cursor.offset += consumed as u64;
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use doubledoor_core::{DoorSessionId, DoorState, GhostDiffId};

    fn state_event(session: DoorSessionId) -> StreamEvent {
        StreamEvent::SessionState {
            session,
            from: DoorState::Active,
            to: DoorState::GhostPending,
        }
    }

    #[test]
    fn sequence_numbers_are_dense_and_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path());
        let s = DoorSessionId::new();
        let mut cursor = EventCursor::default();
        assert!(log.read_new(&mut cursor).unwrap().is_empty());

        log.append(state_event(s)).unwrap();
        log.append(StreamEvent::GhostAssessed {
            session: s,
            ghost: GhostDiffId::new(),
            decision: "deferred".to_string(),
            by: None,
        })
        .unwrap();
        let first = log.read_new(&mut cursor).unwrap();
        assert_eq!(first.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
        assert!(log.read_new(&mut cursor).unwrap().is_empty());

        log.append(state_event(s)).unwrap();
        let next = log.read_new(&mut cursor).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].seq, 3);
        assert_eq!(cursor.seq(), 3);
    }

    #[test]
    fn unterminated_tail_is_not_yet_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path());
        log.append(state_event(DoorSessionId::new())).unwrap();
        // A writer mid-append: partial bytes, no newline yet.
        let mut f = OpenOptions::new().append(true).open(log.path()).unwrap();
        f.write_all(b"{\"schema\":").unwrap();

        let mut cursor = EventCursor::default();
        assert_eq!(log.read_new(&mut cursor).unwrap().len(), 1);
        assert_eq!(cursor.seq(), 1, "the partial line must not be counted");
    }

    #[test]
    fn corrupt_complete_line_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path());
        fs::write(log.path(), b"not json\n").unwrap();
        let mut cursor = EventCursor::default();
        assert!(matches!(
            log.read_new(&mut cursor),
            Err(JournalError::Corrupt { .. })
        ));
    }
}
