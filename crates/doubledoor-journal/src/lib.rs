//! The per-session write-ahead journal (§24/§25, Phase I subset).
//!
//! Every state change is appended as one line — an
//! [`Envelope`]`<`[`JournalRecord`]`>` in JSON — and made durable with
//! flush + `sync_all` before it is considered applied (§24). Prior lines are
//! never rewritten; recovery replays them in order, tolerating a torn final
//! line (§26).
//!
//! Store-path convention: under a store root (typically `.doubledoor`) the
//! layout is `sessions/<session_id>/metadata.cbor` plus
//! `sessions/<session_id>/journal/events.jsonl`. This crate is store-path
//! agnostic: [`Journal::open`], [`replay`], [`save_metadata`], and
//! [`load_metadata`] all take the *session directory*
//! (`sessions/<session_id>`), while [`list_session_dirs`] takes the *sessions
//! root* (`sessions/`). The caller composes them with whatever store root it
//! manages.

#![forbid(unsafe_code)]

mod error;
mod events;
mod record;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use doubledoor_core::{DoorSession, DoorSessionId, GhostDecision, GhostDiffId};
use doubledoor_protocol::Envelope;

pub use error::JournalError;
pub use events::{EventCursor, EventLog};
pub use record::{JournalEventKind, JournalRecord};

const JOURNAL_DIR: &str = "journal";
const EVENTS_FILE: &str = "events.jsonl";
const METADATA_FILE: &str = "metadata.cbor";
const LOCK_FILE: &str = "lock";

/// The result of [`replay`]: every record up to the first corrupt line, plus
/// whether the journal ended in a torn (partially written) tail line.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayReport {
    pub records: Vec<JournalRecord>,
    pub torn_tail: bool,
}

/// An append-only handle to a session's `journal/events.jsonl`.
#[derive(Debug)]
pub struct Journal {
    file: File,
}

impl Journal {
    /// Open (or create) the journal inside `session_dir`. Creates
    /// `session_dir/journal/` and an empty `events.jsonl` if absent.
    pub fn open(session_dir: &Path) -> Result<Self, JournalError> {
        ensure_session_dir(session_dir)?;
        let journal_dir = session_dir.join(JOURNAL_DIR);
        fs::create_dir_all(&journal_dir)?;
        let path = journal_dir.join(EVENTS_FILE);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Journal { file })
    }

    /// Append one record. Serialized as an `Envelope<JournalRecord>` JSON
    /// line, flushed and `sync_all`'ed before returning (§24 durability);
    /// the record is considered applied only once this call succeeds.
    pub fn append(&mut self, record: JournalRecord) -> Result<(), JournalError> {
        let line = serde_json::to_string(&Envelope::new(record))?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }
}

/// Replay `session_dir/journal/events.jsonl` in order. Blank lines are
/// skipped. A torn final line (invalid JSON) is tolerated: replay stops there
/// and reports `torn_tail: true`. A corrupt line anywhere else is a hard
/// [`JournalError::Corrupt`]. Every envelope's schema is checked (§44).
pub fn replay(session_dir: &Path) -> Result<ReplayReport, JournalError> {
    let path = session_dir.join(JOURNAL_DIR).join(EVENTS_FILE);
    if !path.exists() {
        return Ok(ReplayReport {
            records: Vec::new(),
            torn_tail: false,
        });
    }
    let reader = BufReader::new(File::open(&path)?);
    let mut records = Vec::new();
    let mut lines = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        lines.push((idx + 1, line?));
    }
    let mut torn_tail = false;
    let last_line_no = lines
        .iter()
        .rev()
        .find(|(_, l)| !l.trim().is_empty())
        .map(|(n, _)| *n);
    for (line_no, raw) in lines {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Envelope<JournalRecord>>(&raw) {
            Ok(env) => {
                env.check_schema()
                    .map_err(|e| JournalError::UnsupportedSchema(e.to_string()))?;
                records.push(env.payload);
            }
            Err(e) => {
                if Some(line_no) == last_line_no {
                    torn_tail = true;
                    break;
                }
                return Err(JournalError::Corrupt {
                    line: line_no,
                    reason: e.to_string(),
                });
            }
        }
    }
    Ok(ReplayReport { records, torn_tail })
}

/// Exclusive advisory lock on one session (`<session_dir>/lock`), held for
/// the duration of any read-modify-write of its metadata and journal, so a
/// `watch` pass and a concurrent `accept` never overwrite each other's state
/// change. Released on drop (or process exit). Per-session, so contention
/// on one session never blocks another (DD-I7).
#[derive(Debug)]
pub struct SessionLock {
    _file: File,
}

impl SessionLock {
    /// Block until the lock on `session_dir` is held.
    pub fn acquire(session_dir: &Path) -> Result<Self, JournalError> {
        ensure_session_dir(session_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(session_dir.join(LOCK_FILE))?;
        file.lock()?;
        Ok(SessionLock { _file: file })
    }

    /// Take the lock only if it is free right now; `None` when another
    /// operation (a validation run, a promotion) holds it.
    pub fn try_acquire(session_dir: &Path) -> Result<Option<Self>, JournalError> {
        ensure_session_dir(session_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(session_dir.join(LOCK_FILE))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(SessionLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

/// Ghosts whose record has been entered into the journal
/// ([`JournalEventKind::GhostRecorded`]).
pub fn recorded_ghosts(records: &[JournalRecord]) -> HashSet<GhostDiffId> {
    records
        .iter()
        .filter_map(|r| match &r.event {
            JournalEventKind::GhostRecorded { ghost, .. } => Some(*ghost),
            _ => None,
        })
        .collect()
}

/// The latest decision per ghost ([`JournalEventKind::GhostAssessed`]); a
/// ghost absent from the map is undecided.
pub fn ghost_decisions(records: &[JournalRecord]) -> HashMap<GhostDiffId, GhostDecision> {
    let mut out = HashMap::new();
    for r in records {
        if let JournalEventKind::GhostAssessed {
            ghost, decision, ..
        } = &r.event
        {
            out.insert(*ghost, decision.clone());
        }
    }
    out
}

/// Persist `session` as `Envelope<DoorSession>` CBOR at
/// `session_dir/metadata.cbor`: temp file in the same directory, fsync,
/// atomic rename over the target (§24).
pub fn save_metadata(session_dir: &Path, session: &DoorSession) -> Result<(), JournalError> {
    ensure_session_dir(session_dir)?;
    let target = session_dir.join(METADATA_FILE);
    let tmp = session_dir.join(format!("{METADATA_FILE}.tmp"));
    {
        let mut file = File::create(&tmp)?;
        ciborium::into_writer(&Envelope::new(session), &mut file)
            .map_err(|e| JournalError::Cbor(e.to_string()))?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &target)?;
    sync_dir(session_dir)?;
    Ok(())
}

/// Load and schema-check `session_dir/metadata.cbor`. A schema newer than
/// this build understands is rejected explicitly, never silently
/// deserialized (§44).
pub fn load_metadata(session_dir: &Path) -> Result<DoorSession, JournalError> {
    let path = session_dir.join(METADATA_FILE);
    let file = File::open(&path)?;
    let env: Envelope<DoorSession> = ciborium::from_reader(file)
        .map_err(|e| JournalError::Cbor(format!("{} ({})", e, path.display())))?;
    env.check_schema()
        .map_err(|e| JournalError::UnsupportedSchema(e.to_string()))?;
    Ok(env.payload)
}

/// Directories under `sessions_root` whose name parses as a
/// [`DoorSessionId`]; anything else is ignored (§27 path hygiene).
pub fn list_session_dirs(sessions_root: &Path) -> Result<Vec<PathBuf>, JournalError> {
    let mut dirs = Vec::new();
    if !sessions_root.exists() {
        return Ok(dirs);
    }
    for entry in fs::read_dir(sessions_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if DoorSessionId::parse(name).is_ok() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn ensure_session_dir(session_dir: &Path) -> Result<(), JournalError> {
    if session_dir.exists() && !session_dir.is_dir() {
        return Err(JournalError::BadSessionDir(session_dir.to_path_buf()));
    }
    fs::create_dir_all(session_dir)?;
    Ok(())
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use doubledoor_core::{
        ActorId, CanonicalState, DoorState, GitOid, IntentDescriptor, PolicySnapshot, SurfaceId,
    };
    use std::time::SystemTime;

    fn temp_session_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "doubledoor-journal-test-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        dir
    }

    fn record(to: DoorState) -> JournalRecord {
        JournalRecord {
            session: DoorSessionId::new(),
            at: SystemTime::UNIX_EPOCH,
            from: DoorState::Requested,
            to,
            event: JournalEventKind::SessionCreated,
        }
    }

    fn sample_session() -> DoorSession {
        DoorSession::new(
            ActorId::new("codex-7").unwrap(),
            IntentDescriptor::new("test intent"),
            CanonicalState {
                commit: GitOid::parse(&"a".repeat(40)).unwrap(),
                tree: GitOid::parse(&"b".repeat(40)).unwrap(),
                captured_at: SystemTime::UNIX_EPOCH,
            },
            vec![SurfaceId::new("crate::router").unwrap()],
            PolicySnapshot::default(),
        )
    }

    #[test]
    fn append_replay_roundtrip() {
        let dir = temp_session_dir("roundtrip");
        let mut journal = Journal::open(&dir).unwrap();
        let a = record(DoorState::Isolating);
        let mut b = record(DoorState::Active);
        b.session = a.session;
        journal.append(a.clone()).unwrap();
        journal.append(b.clone()).unwrap();
        drop(journal);

        let report = replay(&dir).unwrap();
        assert!(!report.torn_tail);
        assert_eq!(report.records, vec![a, b]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn torn_tail_is_tolerated() {
        let dir = temp_session_dir("torn");
        let mut journal = Journal::open(&dir).unwrap();
        journal.append(record(DoorState::Isolating)).unwrap();
        drop(journal);

        // Simulate a crash mid-write: append a truncated line.
        let path = dir.join(JOURNAL_DIR).join(EVENTS_FILE);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"schema\":{\"major\":1,\"minor\":0},\"pay")
            .unwrap();
        f.sync_all().unwrap();
        drop(f);

        let report = replay(&dir).unwrap();
        assert!(report.torn_tail);
        assert_eq!(report.records.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_middle_line_is_hard_error() {
        let dir = temp_session_dir("corrupt");
        let mut journal = Journal::open(&dir).unwrap();
        journal.append(record(DoorState::Isolating)).unwrap();
        journal.append(record(DoorState::Active)).unwrap();
        drop(journal);

        // Rewrite the file with a corrupt first line and a valid second.
        let path = dir.join(JOURNAL_DIR).join(EVENTS_FILE);
        let valid = fs::read_to_string(&path).unwrap();
        let mut lines = valid.lines();
        let second = lines.nth(1).unwrap();
        fs::write(&path, format!("not json at all\n{second}\n")).unwrap();

        let err = replay(&dir).unwrap_err();
        match err {
            JournalError::Corrupt { line, .. } => assert_eq!(line, 1),
            other => panic!("expected Corrupt, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn blank_lines_are_skipped() {
        let dir = temp_session_dir("blanks");
        let mut journal = Journal::open(&dir).unwrap();
        let r = record(DoorState::Isolating);
        journal.append(r.clone()).unwrap();
        drop(journal);

        let path = dir.join(JOURNAL_DIR).join(EVENTS_FILE);
        let mut contents = fs::read_to_string(&path).unwrap();
        contents.push('\n');
        contents.push('\n');
        fs::write(&path, contents).unwrap();

        let report = replay(&dir).unwrap();
        assert!(!report.torn_tail);
        assert_eq!(report.records, vec![r]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metadata_cbor_roundtrip() {
        let dir = temp_session_dir("metadata");
        let session = sample_session();
        save_metadata(&dir, &session).unwrap();
        let loaded = load_metadata(&dir).unwrap();
        assert_eq!(loaded, session);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metadata_rejects_future_schema() {
        use doubledoor_protocol::SchemaVersion;
        let dir = temp_session_dir("schema");
        fs::create_dir_all(&dir).unwrap();
        let session = sample_session();
        let env = Envelope::with_schema(
            SchemaVersion {
                major: doubledoor_protocol::CURRENT_SCHEMA.major + 1,
                minor: 0,
            },
            &session,
        );
        let path = dir.join(METADATA_FILE);
        let mut f = File::create(&path).unwrap();
        ciborium::into_writer(&env, &mut f).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let err = load_metadata(&dir).unwrap_err();
        assert!(
            matches!(err, JournalError::UnsupportedSchema(_)),
            "expected UnsupportedSchema, got {err:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_session_dirs_filters_non_session_names() {
        let root = temp_session_dir("list");
        let id = DoorSessionId::new();
        let good = root.join(id.as_str());
        fs::create_dir_all(&good).unwrap();
        fs::create_dir_all(root.join("not-a-session")).unwrap();
        fs::write(root.join("file.txt"), b"x").unwrap();

        let dirs = list_session_dirs(&root).unwrap();
        assert_eq!(dirs, vec![good]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn open_rejects_non_directory_session_path() {
        let dir = temp_session_dir("baddir");
        fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("a-file");
        fs::write(&file_path, b"x").unwrap();
        let err = Journal::open(&file_path).unwrap_err();
        assert!(matches!(err, JournalError::BadSessionDir(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replay_of_missing_journal_is_empty() {
        let dir = temp_session_dir("empty");
        fs::create_dir_all(&dir).unwrap();
        let report = replay(&dir).unwrap();
        assert!(report.records.is_empty());
        assert!(!report.torn_tail);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
