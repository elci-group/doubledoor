//! The boundary to Locksmith lease authority (§19, Phase I adapter).
//!
//! Doubledoor never invents lease state: it can *receive* a granted lease or
//! a [`ContentionNotice`] from Locksmith, but it cannot forge, bypass, or
//! fabricate Locksmith authority (DD-I6). Phase I ships one concrete
//! [`LockAuthority`]: [`NoticeAdapter`], used when Locksmith has already
//! denied a mutation out-of-band (e.g. delivered via CLI `--notice <json>`).
//! Lease *grant* plumbing arrives with the full Locksmith integration.

#![forbid(unsafe_code)]

use doubledoor_core::{ActorId, ContentionNotice, LeaseId, SurfaceId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The decision Locksmith (or a faithful adapter) returns for a lease
/// request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseDecision {
    Granted { lease_id: LeaseId },
    Contended(ContentionNotice),
}

/// The source of mutation-surface lease authority.
pub trait LockAuthority {
    fn request_lease(
        &self,
        actor: &ActorId,
        surfaces: &[SurfaceId],
    ) -> Result<LeaseDecision, LocksmithError>;
}

/// Phase I adapter for a contention notice Locksmith already produced and
/// delivered out-of-band. Every request contends: Doubledoor never invents a
/// grant it did not receive (DD-I6).
#[derive(Debug, Clone)]
pub struct NoticeAdapter {
    notice: ContentionNotice,
}

impl NoticeAdapter {
    pub fn from_notice(notice: ContentionNotice) -> Self {
        NoticeAdapter { notice }
    }

    pub fn notice(&self) -> &ContentionNotice {
        &self.notice
    }
}

impl LockAuthority for NoticeAdapter {
    fn request_lease(
        &self,
        _actor: &ActorId,
        _surfaces: &[SurfaceId],
    ) -> Result<LeaseDecision, LocksmithError> {
        // DD-I6: Doubledoor never invents a grant it did not receive, so an
        // adapter built from a notice contends on every request. Structural
        // validation of the notice against the requesting actor happens at
        // the use site via `ContentionNotice::validate_for`.
        Ok(LeaseDecision::Contended(self.notice.clone()))
    }
}

/// A Locksmith lease grant authorizing a canonical mutation (§19, §23:
/// "Locksmith permits target mutation"). Doubledoor only ever *receives*
/// grants; it never issues, extends, or releases one (DD-I6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub holder: ActorId,
    pub lease_id: LeaseId,
    /// Surfaces the lease covers: repo-relative paths (`src/router.rs`),
    /// `file:` ids, directory prefixes ending in `/`, or `*` for the whole
    /// repository.
    pub surfaces: Vec<SurfaceId>,
    /// The Locksmith authority that issued it (the signing principal of a
    /// signed lease; Phase VII).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Not valid at or after this instant (ms since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_millis: Option<u64>,
    /// Only valid on these hosts (`trust/host.json` or the hostname).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
}

/// A lease grant signed by a Locksmith authority (§51 distributed leases):
/// an OpenSSH signature, namespace `locksmith-lease`, over the grant's
/// canonical JSON (keys sorted, no whitespace, absent fields omitted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedLease {
    pub grant: LeaseGrant,
    pub signature: String,
}

/// A lease as presented: signed, or a plain grant (single-host mode, only
/// accepted while no Locksmith key is trusted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LeaseDocument {
    Signed(SignedLease),
    Plain(LeaseGrant),
}

impl LeaseDocument {
    pub fn grant(&self) -> &LeaseGrant {
        match self {
            LeaseDocument::Signed(s) => &s.grant,
            LeaseDocument::Plain(g) => g,
        }
    }
}

/// The bytes a Locksmith signs for `grant`.
pub fn grant_payload(grant: &LeaseGrant) -> Vec<u8> {
    doubledoor_trust::canonical_json(&serde_json::to_value(grant).unwrap_or_default())
}

/// How a lease was established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseCheck {
    pub lease_id: LeaseId,
    pub holder: ActorId,
    /// The verified Locksmith signer, when the lease was signed and a
    /// Locksmith key is trusted here.
    pub signed_by: Option<doubledoor_trust::Verified>,
    pub host: String,
}

/// Every check a lease must pass before it authorizes a canonical mutation
/// on this host: signature (required once a Locksmith key is trusted),
/// expiry, host binding, holder, and coverage of every file.
pub fn check_lease(
    doc: &LeaseDocument,
    store: &doubledoor_trust::TrustStore,
    now_millis: u64,
    actor: &ActorId,
    files: &[String],
) -> Result<LeaseCheck, LocksmithError> {
    let grant = doc.grant();
    let host = store.host_id();
    let signed_by = match (doc, store.requires_signed_leases()) {
        (LeaseDocument::Plain(_), true) => {
            return Err(LocksmithError::NotPermitted(
                "this host trusts a Locksmith key; the lease must be signed".to_string(),
            ))
        }
        (LeaseDocument::Signed(s), true) => {
            let issuer = grant.issuer.as_deref().ok_or_else(|| {
                LocksmithError::NotPermitted("signed lease names no issuer".to_string())
            })?;
            let v = doubledoor_trust::verify(
                &store.locksmith_signers(),
                issuer,
                doubledoor_trust::NS_LEASE,
                &grant_payload(grant),
                &s.signature,
                &store.scratch(),
            )
            .map_err(|e| LocksmithError::NotPermitted(format!("lease signature: {e}")))?;
            Some(v)
        }
        (_, false) => None,
    };
    if let Some(t) = grant.expires_at_millis {
        if now_millis >= t {
            return Err(LocksmithError::NotPermitted(format!(
                "lease {} expired",
                grant.lease_id
            )));
        }
    }
    if let Some(hosts) = &grant.hosts {
        if !hosts.iter().any(|h| h == &host) {
            return Err(LocksmithError::NotPermitted(format!(
                "lease {} is bound to {hosts:?}, this host is `{host}`",
                grant.lease_id
            )));
        }
    }
    authorize_mutation(grant, actor, files)?;
    Ok(LeaseCheck {
        lease_id: grant.lease_id,
        holder: grant.holder.clone(),
        signed_by,
        host,
    })
}

pub fn parse_grant(json: &str) -> Result<LeaseGrant, LocksmithError> {
    serde_json::from_str(json).map_err(LocksmithError::from)
}

/// Check that `grant` lets `actor` mutate every path in `files`. Returns the
/// uncovered paths as an error — a partial lease never authorizes a
/// promotion.
pub fn authorize_mutation(
    grant: &LeaseGrant,
    actor: &ActorId,
    files: &[String],
) -> Result<(), LocksmithError> {
    if &grant.holder != actor {
        return Err(LocksmithError::NotPermitted(format!(
            "lease {} is held by {}, not {actor}",
            grant.lease_id, grant.holder
        )));
    }
    let covers = |path: &str| {
        grant.surfaces.iter().any(|s| {
            let s = s.as_str();
            let s = s.strip_prefix("file:").unwrap_or(s);
            s == "*" || s == path || (s.ends_with('/') && path.starts_with(s))
        })
    };
    let uncovered: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|f| !covers(f))
        .collect();
    if uncovered.is_empty() {
        Ok(())
    } else {
        Err(LocksmithError::NotPermitted(format!(
            "lease {} does not cover: {}",
            grant.lease_id,
            uncovered.join(", ")
        )))
    }
}

/// Parse a JSON contention notice (e.g. from `--notice <json>`). Structural
/// validation against the requesting actor happens at use time via
/// [`ContentionNotice::validate_for`] (or implicitly through
/// [`NoticeAdapter`]).
pub fn parse_notice(json: &str) -> Result<ContentionNotice, LocksmithError> {
    serde_json::from_str(json).map_err(LocksmithError::from)
}

#[derive(Debug, Error)]
pub enum LocksmithError {
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error("invalid contention notice: {0}")]
    InvalidNotice(String),
    /// The lock authority does not permit the requested mutation.
    #[error("not permitted by Locksmith: {0}")]
    NotPermitted(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use doubledoor_core::GitOid;

    fn notice() -> ContentionNotice {
        ContentionNotice {
            holder: ActorId::new("agent-alpha").unwrap(),
            requested: vec![SurfaceId::new("src/router.rs").unwrap()],
            contested: vec![SurfaceId::new("src/router.rs").unwrap()],
            lease_id: LeaseId::new(),
            base_commit: GitOid::parse(&"a".repeat(40)).unwrap(),
        }
    }

    #[test]
    fn from_notice_roundtrips_through_json() {
        let n = notice();
        let json = serde_json::to_string(&n).unwrap();
        let parsed = parse_notice(&json).unwrap();
        assert_eq!(parsed, n);

        let adapter = NoticeAdapter::from_notice(parsed.clone());
        assert_eq!(adapter.notice(), &parsed);
    }

    #[test]
    fn adapter_always_contends_never_grants() {
        let adapter = NoticeAdapter::from_notice(notice());
        let decision = adapter
            .request_lease(
                &ActorId::new("agent-beta").unwrap(),
                &[SurfaceId::new("src/router.rs").unwrap()],
            )
            .unwrap();
        match decision {
            LeaseDecision::Contended(n) => assert_eq!(n, *adapter.notice()),
            LeaseDecision::Granted { .. } => panic!("NoticeAdapter must never grant"),
        }
    }

    #[test]
    fn parse_notice_rejects_garbage() {
        assert!(parse_notice("not json").is_err());
        assert!(parse_notice("{}").is_err());
        assert!(parse_notice("[]").is_err());
    }

    #[test]
    fn grants_must_be_held_by_the_actor_and_cover_every_path() {
        let beta = ActorId::new("agent-beta").unwrap();
        let grant = LeaseGrant {
            holder: beta.clone(),
            lease_id: LeaseId::new(),
            surfaces: vec![
                SurfaceId::new("src/handler.rs").unwrap(),
                SurfaceId::new("file:src/retry.rs").unwrap(),
                SurfaceId::new("docs/").unwrap(),
            ],
            issuer: None,
            expires_at_millis: None,
            hosts: None,
        };
        let files = |f: &[&str]| f.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(authorize_mutation(
            &grant,
            &beta,
            &files(&["src/handler.rs", "src/retry.rs", "docs/a.md"])
        )
        .is_ok());
        let err = authorize_mutation(&grant, &beta, &files(&["src/handler.rs", "src/router.rs"]))
            .unwrap_err();
        assert!(err.to_string().contains("src/router.rs"));
        let alpha = ActorId::new("agent-alpha").unwrap();
        assert!(authorize_mutation(&grant, &alpha, &files(&["src/handler.rs"])).is_err());
        let all = LeaseGrant {
            surfaces: vec![SurfaceId::new("*").unwrap()],
            ..grant
        };
        assert!(authorize_mutation(&all, &beta, &files(&["anything"])).is_ok());
    }
}
