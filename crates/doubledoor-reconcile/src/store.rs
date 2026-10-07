//! Candidate persistence: `<session dir>/candidates/<id>.json`, each an
//! [`Envelope`]`<`[`Candidate`]`>`, written atomically (temp → fsync →
//! rename → dir fsync, §24) and schema-checked on read (§44).

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use doubledoor_core::CandidateId;
use doubledoor_protocol::Envelope;

use crate::candidate::{candidate_dir, Candidate};
use crate::error::ReconcileError;

pub fn save(session_dir: &Path, c: &Candidate) -> Result<(), ReconcileError> {
    save_json(&candidate_dir(session_dir), &c.id.as_str(), c)
}

pub fn load(session_dir: &Path, id: &CandidateId) -> Result<Candidate, ReconcileError> {
    load_json(&candidate_dir(session_dir), &id.as_str())
}

/// Atomically write `Envelope<T>` as `<dir>/<name>.json`.
pub fn save_json<T: serde::Serialize>(
    dir: &Path,
    name: &str,
    value: &T,
) -> Result<(), ReconcileError> {
    fs::create_dir_all(dir)?;
    let target = dir.join(format!("{name}.json"));
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut f = File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut f, &Envelope::new(value))
            .map_err(|e| ReconcileError::Corrupt(e.to_string()))?;
        f.flush()?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &target)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Read and schema-check `<dir>/<name>.json`.
pub fn load_json<T: serde::de::DeserializeOwned>(
    dir: &Path,
    name: &str,
) -> Result<T, ReconcileError> {
    let path = dir.join(format!("{name}.json"));
    let env: Envelope<T> = serde_json::from_reader(File::open(&path)?)
        .map_err(|e| ReconcileError::Corrupt(format!("{} ({e})", path.display())))?;
    env.check_schema()
        .map_err(|e| ReconcileError::Corrupt(e.to_string()))?;
    Ok(env.payload)
}
