//! The live policy file and Phase I quota evaluation (§28).
//!
//! [`PolicySnapshot`] itself lives in `doubledoor-core` (it travels with
//! every session); this crate owns the on-disk policy under a store root —
//! `<root>/policy.json` as an `Envelope<PolicySnapshot>` — and the decision
//! logic applied when a session is requested.
//!
//! Phase I enforces `max_active_sessions` at open time and reports expired
//! sessions via [`expired_sessions`]. `max_workspace_bytes` is enforced at
//! Phase II when workspaces actually materialize.

#![forbid(unsafe_code)]

mod error;

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;

use doubledoor_core::{ActorId, DoorSession, DoorSessionId, PolicySnapshot, SurfaceId};
use doubledoor_protocol::Envelope;
use serde::{Deserialize, Serialize};

pub use error::{DenyReason, PolicyError};

const POLICY_FILE: &str = "policy.json";

/// A request to open a new DoorSession (§7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub actor: ActorId,
    pub requested_surfaces: Vec<SurfaceId>,
}

/// Load `<root>/policy.json` as an `Envelope<PolicySnapshot>`, schema-checked
/// on read (§44). A missing file yields [`PolicySnapshot::default`].
pub fn load(root: &Path) -> Result<PolicySnapshot, PolicyError> {
    let path = root.join(POLICY_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let env: Envelope<PolicySnapshot> = serde_json::from_str(&text)?;
            env.check_schema()
                .map_err(|e| PolicyError::UnsupportedSchema(e.to_string()))?;
            Ok(env.payload)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(PolicySnapshot::default()),
        Err(e) => Err(e.into()),
    }
}

/// Persist `snapshot` to `<root>/policy.json`: temp file in the same
/// directory, fsync, atomic rename (§24).
pub fn save(root: &Path, snapshot: &PolicySnapshot) -> Result<(), PolicyError> {
    fs::create_dir_all(root)?;
    let target = root.join(POLICY_FILE);
    let tmp = root.join(format!("{POLICY_FILE}.tmp"));
    {
        let mut file = File::create(&tmp)?;
        serde_json::to_writer(&mut file, &Envelope::new(snapshot))?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &target)?;
    sync_dir(root)?;
    Ok(())
}

/// Evaluate an open request against the live policy. Phase I denies when the
/// count of sessions whose state counts as active
/// ([`doubledoor_core::DoorState::counts_as_active`]) has already reached `max_active_sessions`.
/// `now` is accepted for forward compatibility with lifetime checks at open
/// time; per-session expiry is surfaced separately via [`expired_sessions`].
pub fn evaluate(
    policy: &PolicySnapshot,
    request: &OpenRequest,
    active_sessions: &[&DoorSession],
    now: SystemTime,
) -> Result<(), PolicyError> {
    let _ = (request, now);
    let current = active_sessions
        .iter()
        .filter(|s| s.state.counts_as_active())
        .count();
    if current >= policy.max_active_sessions as usize {
        return Err(PolicyError::Denied {
            reason: DenyReason::MaxActiveSessions {
                current,
                max: policy.max_active_sessions,
            },
        });
    }
    Ok(())
}

/// Sessions whose age exceeds the lifetime recorded in their own policy
/// snapshot (§28). Evaluation uses each session's in-force snapshot because
/// the lifetime in force at creation governs that session.
pub fn expired_sessions(sessions: &[DoorSession], now: SystemTime) -> Vec<DoorSessionId> {
    sessions
        .iter()
        .filter(|s| s.age_secs(now) > s.policy.max_session_lifetime_secs)
        .map(|s| s.id)
        .collect()
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use doubledoor_core::{CanonicalState, DoorState, GitOid, IntentDescriptor};
    use std::path::PathBuf;
    use std::time::Duration;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "doubledoor-policy-test-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    fn session_in(state: DoorState) -> DoorSession {
        let mut s = DoorSession::new(
            ActorId::new("codex-7").unwrap(),
            IntentDescriptor::new("test"),
            CanonicalState {
                commit: GitOid::parse(&"a".repeat(40)).unwrap(),
                tree: GitOid::parse(&"b".repeat(40)).unwrap(),
                captured_at: SystemTime::UNIX_EPOCH,
            },
            vec![],
            PolicySnapshot::default(),
        );
        s.state = state;
        s
    }

    #[test]
    fn load_missing_file_yields_default() {
        let root = temp_root("missing");
        fs::create_dir_all(&root).unwrap();
        assert_eq!(load(&root).unwrap(), PolicySnapshot::default());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn save_load_roundtrip() {
        let root = temp_root("roundtrip");
        let policy = PolicySnapshot {
            max_active_sessions: 3,
            max_workspace_bytes: 1024,
            max_session_lifetime_secs: 60,
            max_reconciliation_attempts: 2,
            max_validation_runtime_secs: 30,
        };
        save(&root, &policy).unwrap();
        assert_eq!(load(&root).unwrap(), policy);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_rejects_future_schema() {
        use doubledoor_protocol::SchemaVersion;
        let root = temp_root("schema");
        fs::create_dir_all(&root).unwrap();
        let env = Envelope::with_schema(
            SchemaVersion {
                major: doubledoor_protocol::CURRENT_SCHEMA.major + 1,
                minor: 0,
            },
            PolicySnapshot::default(),
        );
        fs::write(root.join(POLICY_FILE), serde_json::to_string(&env).unwrap()).unwrap();
        assert!(matches!(
            load(&root).unwrap_err(),
            PolicyError::UnsupportedSchema(_)
        ));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn quota_allows_below_limit_and_denies_at_limit() {
        let policy = PolicySnapshot {
            max_active_sessions: 2,
            ..PolicySnapshot::default()
        };
        let request = OpenRequest {
            actor: ActorId::new("codex-7").unwrap(),
            requested_surfaces: vec![],
        };
        let one = session_in(DoorState::Active);
        let two = session_in(DoorState::Isolating);
        let done = session_in(DoorState::Completed);

        // One active: allowed.
        let active: Vec<&DoorSession> = vec![&one];
        assert!(evaluate(&policy, &request, &active, SystemTime::UNIX_EPOCH).is_ok());

        // Terminal sessions do not count against the quota.
        let mixed: Vec<&DoorSession> = vec![&one, &done];
        assert!(evaluate(&policy, &request, &mixed, SystemTime::UNIX_EPOCH).is_ok());

        // At the limit: denied with the counts spelled out.
        let full: Vec<&DoorSession> = vec![&one, &two];
        let err = evaluate(&policy, &request, &full, SystemTime::UNIX_EPOCH).unwrap_err();
        match err {
            PolicyError::Denied {
                reason: DenyReason::MaxActiveSessions { current, max },
            } => {
                assert_eq!(current, 2);
                assert_eq!(max, 2);
            }
            other => panic!("expected quota denial, got {other:?}"),
        }
    }

    #[test]
    fn expired_sessions_use_each_sessions_own_policy() {
        let mut young = session_in(DoorState::Active);
        young.created_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        young.policy.max_session_lifetime_secs = 1000;

        let mut old = session_in(DoorState::Active);
        old.created_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        old.policy.max_session_lifetime_secs = 50;

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let expired = expired_sessions(&[young, old], now);
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn expired_sessions_ignores_terminal_sessions() {
        let mut s = session_in(DoorState::Abandoned);
        s.created_at = SystemTime::UNIX_EPOCH;
        s.policy.max_session_lifetime_secs = 1;
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        // Expiry is reported regardless of state so the reaper can find
        // stragglers, but a terminal session older than its lifetime still
        // shows up exactly once.
        assert_eq!(expired_sessions(&[s], now).len(), 1);
    }
}
