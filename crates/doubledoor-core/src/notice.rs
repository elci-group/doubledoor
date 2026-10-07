//! Contention and intent vocabulary shared with Locksmith (§19).

use serde::{Deserialize, Serialize};

use crate::{ActorId, GitOid, LeaseId, SurfaceId};

/// What an actor intends to do, in their own words (§7). Free-form, but the
/// descriptor itself is structured so intent travels with every session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentDescriptor {
    pub summary: String,
}

impl IntentDescriptor {
    pub fn new(summary: impl Into<String>) -> Self {
        IntentDescriptor {
            summary: summary.into(),
        }
    }
}

/// Structured contention information Locksmith returns when a mutation
/// request cannot be granted (§19). Doubledoor only ever *receives* these;
/// it never fabricates lease state (DD-I6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentionNotice {
    pub holder: ActorId,
    pub requested: Vec<SurfaceId>,
    pub contested: Vec<SurfaceId>,
    pub lease_id: LeaseId,
    pub base_commit: GitOid,
}

impl ContentionNotice {
    /// Structural validation before a notice is trusted: the holder must not
    /// be the requesting actor, and there must be an actual intersection
    /// between requested and contested surfaces.
    pub fn validate_for(&self, requester: &ActorId) -> Result<(), NoticeError> {
        if &self.holder == requester {
            return Err(NoticeError::HolderIsRequester);
        }
        if self.requested.is_empty() {
            return Err(NoticeError::EmptyRequested);
        }
        if self.contested.is_empty() {
            return Err(NoticeError::NoContestedSurfaces);
        }
        let intersects = self
            .requested
            .iter()
            .any(|r| self.contested.iter().any(|c| c == r));
        if !intersects {
            return Err(NoticeError::NoIntersection);
        }
        Ok(())
    }
}

/// Why a contention notice was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NoticeError {
    #[error("notice holder equals the requesting actor")]
    HolderIsRequester,
    #[error("notice contains no requested surfaces")]
    EmptyRequested,
    #[error("notice contains no contested surfaces")]
    NoContestedSurfaces,
    #[error("requested and contested surface sets do not intersect")]
    NoIntersection,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(s: &str) -> SurfaceId {
        SurfaceId::new(s).unwrap()
    }

    fn notice() -> ContentionNotice {
        ContentionNotice {
            holder: ActorId::new("agent-alpha").unwrap(),
            requested: vec![surface("src/router.rs")],
            contested: vec![surface("src/router.rs")],
            lease_id: LeaseId::new(),
            base_commit: GitOid::parse(&"a".repeat(40)).unwrap(),
        }
    }

    #[test]
    fn valid_notice_passes() {
        let n = notice();
        n.validate_for(&ActorId::new("agent-beta").unwrap())
            .unwrap();
    }

    #[test]
    fn holder_cannot_be_requester() {
        let n = notice();
        let err = n.validate_for(&ActorId::new("agent-alpha").unwrap());
        assert_eq!(err.unwrap_err(), NoticeError::HolderIsRequester);
    }

    #[test]
    fn non_intersecting_notice_is_rejected() {
        let mut n = notice();
        n.contested = vec![surface("src/other.rs")];
        let err = n.validate_for(&ActorId::new("agent-beta").unwrap());
        assert_eq!(err.unwrap_err(), NoticeError::NoIntersection);
    }

    #[test]
    fn empty_contested_is_rejected() {
        let mut n = notice();
        n.contested = vec![];
        let err = n.validate_for(&ActorId::new("agent-beta").unwrap());
        assert_eq!(err.unwrap_err(), NoticeError::NoContestedSurfaces);
    }
}
