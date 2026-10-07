//! Per-session ghost persistence (§12).
//!
//! GhostDiffs live at `.doubledoor/sessions/<session_id>/ghosts/<ghost_id>.json`
//! under the repository root, each a schema-versioned
//! [`Envelope`]`<`[`GhostDiff`]`>`. Writes are atomic (temp file in the same
//! directory → fsync → rename → directory fsync, §24). Reads check the
//! schema explicitly — a record from a newer build is rejected, never
//! silently assumed compatible (§44).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use doubledoor_core::{DoorSessionId, GhostDiffId};
use doubledoor_git::Git;
use doubledoor_protocol::Envelope;

use crate::error::GhostError;
use crate::ghost::GhostDiff;

/// The ghosts directory name inside a session directory.
const GHOSTS_DIR: &str = "ghosts";

/// Append-only store of a session's ghost records on disk.
#[derive(Debug, Clone)]
pub struct GhostStore {
    dir: PathBuf,
}

impl GhostStore {
    /// The store for `session` under `git`'s repository root. Performs no
    /// I/O until the first save/load.
    pub fn new(git: &Git, session: &DoorSessionId) -> Result<Self, GhostError> {
        let id = session.as_str();
        // Defense in depth (§27): DoorSessionId renders as dd_<uuid> and is
        // filesystem-safe by construction; the check mirrors
        // doubledoor-git::Git::session_dir.
        if id.is_empty() || id.contains(['/', '\\', '\0']) || id.contains("..") {
            return Err(GhostError::UnexpectedOutput(format!(
                "session id is not filesystem-safe: {id}"
            )));
        }
        Ok(GhostStore {
            dir: git
                .repo_root()
                .join(".doubledoor/sessions")
                .join(id)
                .join(GHOSTS_DIR),
        })
    }

    /// The directory ghost records are stored in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Persist one ghost record: temp file → fsync → atomic rename →
    /// directory fsync (§24). Overwrites are impossible in practice (fresh
    /// `GhostDiffId`s), but rename semantics make re-saving safe regardless.
    pub fn save(&self, ghost: &GhostDiff) -> Result<(), GhostError> {
        fs::create_dir_all(&self.dir)?;
        let target = self.dir.join(format!("{}.json", ghost.id.as_str()));
        let tmp = self.dir.join(format!(".{}.tmp", ghost.id.as_str()));
        {
            let mut file = File::create(&tmp)?;
            serde_json::to_writer(&mut file, &Envelope::new(ghost))
                .map_err(|e| GhostError::Corrupt(e.to_string()))?;
            file.flush()?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &target)?;
        sync_dir(&self.dir)?;
        Ok(())
    }

    /// Load and schema-check one ghost record.
    pub fn load(&self, id: &GhostDiffId) -> Result<GhostDiff, GhostError> {
        let path = self.dir.join(format!("{}.json", id.as_str()));
        let file = File::open(&path)?;
        let env: Envelope<GhostDiff> = serde_json::from_reader(file)
            .map_err(|e| GhostError::Corrupt(format!("{} ({})", e, path.display())))?;
        env.check_schema()?;
        Ok(env.payload)
    }

    /// Every persisted ghost, sorted by id for deterministic output.
    pub fn list(&self) -> Result<Vec<GhostDiff>, GhostError> {
        let mut ghosts = Vec::new();
        if !self.dir.is_dir() {
            return Ok(ghosts);
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(id) = GhostDiffId::parse(stem) else {
                continue;
            };
            ghosts.push(self.load(&id)?);
        }
        ghosts.sort_by_key(|g| g.id.as_str());
        Ok(ghosts)
    }
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}
