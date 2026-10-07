//! Domain identity newtypes (§41: never interchange arbitrary strings where
//! domain identity matters).

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

use crate::error::IdError;

/// Serde for the prefixed uuid ids: always *written* as the canonical
/// `<prefix><uuid>` string, so every id an agent reads from `--format json`
/// or the event stream can be passed straight back to the CLI. *Reading*
/// also accepts the forms earlier builds wrote — a bare uuid string (JSON)
/// or 16 raw bytes (CBOR metadata) — so existing sessions stay loadable.
macro_rules! prefixed_uuid_serde {
    ($ty:ident, $prefix:literal) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl serde::de::Visitor<'_> for V {
                    type Value = $ty;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "a {}<uuid> id", $prefix)
                    }
                    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<$ty, E> {
                        let raw = v.strip_prefix($prefix).unwrap_or(v);
                        Uuid::parse_str(raw).map($ty).map_err(E::custom)
                    }
                    fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<$ty, E> {
                        Uuid::from_slice(v).map($ty).map_err(E::custom)
                    }
                }
                d.deserialize_any(V)
            }
        }
    };
}

prefixed_uuid_serde!(DoorSessionId, "dd_");
prefixed_uuid_serde!(GhostDiffId, "gh_");
prefixed_uuid_serde!(CandidateId, "cand_");

/// Unique identifier of a reconciliation candidate (§16). Rendered as
/// `cand_<uuid>`; used as an on-disk file name, so parsing is strict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CandidateId(Uuid);

impl CandidateId {
    pub fn new() -> Self {
        CandidateId(Uuid::new_v4())
    }

    /// Parse from the canonical `cand_<uuid>` rendering.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let raw = s
            .strip_prefix("cand_")
            .ok_or_else(|| IdError::BadPrefix(s.to_string()))?;
        let uuid = Uuid::parse_str(raw).map_err(|_| IdError::BadUuid(s.to_string()))?;
        Ok(CandidateId(uuid))
    }

    pub fn as_str(&self) -> String {
        format!("cand_{}", self.0.as_hyphenated())
    }
}

impl Default for CandidateId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CandidateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// Unique identifier of a DoorSession. Rendered as `dd_<uuid>` and used,
/// among other things, as the on-disk directory name — which is why parsing
/// is strict (§27, path hygiene).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DoorSessionId(Uuid);

impl DoorSessionId {
    pub fn new() -> Self {
        DoorSessionId(Uuid::new_v4())
    }

    /// Parse from the canonical `dd_<uuid>` rendering.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let raw = s
            .strip_prefix("dd_")
            .ok_or_else(|| IdError::BadPrefix(s.to_string()))?;
        let uuid = Uuid::parse_str(raw).map_err(|_| IdError::BadUuid(s.to_string()))?;
        Ok(DoorSessionId(uuid))
    }

    /// The canonical `dd_<uuid>` rendering. Always filesystem-safe.
    pub fn as_str(&self) -> String {
        format!("dd_{}", self.0.as_hyphenated())
    }
}

impl Default for DoorSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for DoorSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// Stable session-scoped identifier of an actor (§5.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActorId(String);

impl ActorId {
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() || s.len() > 256 || s.contains(['/', '\\', '\0']) {
            return Err(IdError::BadActor(s));
        }
        Ok(ActorId(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A mutation surface identifier (§5.3). Phase II mints file-level ids
/// (`file:<path>`); a Phase III symbol-level resolver slots in behind the same
/// type. Ordering is lexicographic so surface collections serialize
/// deterministically (§3).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfaceId(String);

impl SurfaceId {
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() || s.len() > 1024 || s.contains('\0') {
            return Err(IdError::BadSurface(s));
        }
        Ok(SurfaceId(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SurfaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Git object identifier (commit or tree). Validated as lowercase hex of
/// 40 (SHA-1) or 64 (SHA-256) characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitOid(String);

impl GitOid {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let ok = matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
        if !ok {
            return Err(IdError::BadOid(s.to_string()));
        }
        Ok(GitOid(s.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identifier of a Locksmith lease (§19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseId(Uuid);

impl LeaseId {
    pub fn new() -> Self {
        LeaseId(Uuid::new_v4())
    }
}

impl Default for LeaseId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for LeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.as_hyphenated())
    }
}

/// Unique identifier of a GhostDiff (§12). Rendered as `gh_<uuid>` and used as
/// the on-disk file name of a persisted ghost record, so parsing is strict
/// (§27, path hygiene).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GhostDiffId(Uuid);

impl GhostDiffId {
    pub fn new() -> Self {
        GhostDiffId(Uuid::new_v4())
    }

    /// Parse from the canonical `gh_<uuid>` rendering.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let raw = s
            .strip_prefix("gh_")
            .ok_or_else(|| IdError::BadPrefix(s.to_string()))?;
        let uuid = Uuid::parse_str(raw).map_err(|_| IdError::BadUuid(s.to_string()))?;
        Ok(GhostDiffId(uuid))
    }

    /// The canonical `gh_<uuid>` rendering. Always filesystem-safe.
    pub fn as_str(&self) -> String {
        format!("gh_{}", self.0.as_hyphenated())
    }
}

impl Default for GhostDiffId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for GhostDiffId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_roundtrip_is_filesystem_safe() {
        let id = DoorSessionId::new();
        let s = id.as_str();
        assert!(s.starts_with("dd_"));
        assert!(s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(DoorSessionId::parse(&s).unwrap(), id);
    }

    #[test]
    fn session_id_rejects_traversal() {
        assert!(DoorSessionId::parse("../etc").is_err());
        assert!(DoorSessionId::parse("dd_../../x").is_err());
        assert!(DoorSessionId::parse("nope").is_err());
    }

    #[test]
    fn actor_id_rejects_empty_and_pathish() {
        assert!(ActorId::new("").is_err());
        assert!(ActorId::new("a/b").is_err());
        assert!(ActorId::new("codex-7").is_ok());
    }

    #[test]
    fn oid_validation() {
        assert!(GitOid::parse(&"a".repeat(40)).is_ok());
        assert!(GitOid::parse(&"b".repeat(64)).is_ok());
        assert!(GitOid::parse(&"c".repeat(41)).is_err());
        assert!(GitOid::parse("g".repeat(40).as_str()).is_err());
        assert!(GitOid::parse("").is_err());
    }

    #[test]
    fn oid_normalizes_case() {
        let upper: String = "A".repeat(40);
        assert_eq!(GitOid::parse(&upper).unwrap().as_str(), "a".repeat(40));
    }

    #[test]
    fn ids_serialize_prefixed_and_read_legacy_forms() {
        let id = DoorSessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<DoorSessionId>(&json).unwrap(), id);
        // Bare uuid string, as JSON written by earlier builds.
        let bare = format!("\"{}\"", id.0.as_hyphenated());
        assert_eq!(serde_json::from_str::<DoorSessionId>(&bare).unwrap(), id);
        // 16 raw bytes, as CBOR metadata written by earlier builds.
        let mut cbor = Vec::new();
        ciborium::into_writer(&serde_bytes_like(id.0.as_bytes()), &mut cbor).unwrap();
        let back: DoorSessionId = ciborium::from_reader(cbor.as_slice()).unwrap();
        assert_eq!(back, id);

        let g = GhostDiffId::new();
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.starts_with("\"gh_"));
        assert_eq!(serde_json::from_str::<GhostDiffId>(&json).unwrap(), g);
        assert!(serde_json::from_str::<GhostDiffId>("\"gh_nope\"").is_err());
    }

    /// A CBOR byte string (what `Uuid`'s non-human-readable serde emits).
    fn serde_bytes_like(b: &[u8]) -> ciborium::Value {
        ciborium::Value::Bytes(b.to_vec())
    }

    #[test]
    fn ghost_diff_id_roundtrip_is_filesystem_safe() {
        let id = GhostDiffId::new();
        let s = id.as_str();
        assert!(s.starts_with("gh_"));
        assert!(s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(GhostDiffId::parse(&s).unwrap(), id);
        assert!(GhostDiffId::parse("dd_not-a-ghost").is_err());
        assert!(GhostDiffId::parse("../etc").is_err());
    }
}
