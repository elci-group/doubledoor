//! Typed domain errors (§41: libraries avoid opaque stringly-typed errors).

use thiserror::Error;

/// Errors while constructing or parsing domain identifiers.
// The `Bad*` prefix is the vocabulary of this error type; renaming the
// variants would churn every caller for no behavioral gain.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdError {
    #[error("session id must start with `dd_`: {0}")]
    BadPrefix(String),
    #[error("invalid session uuid: {0}")]
    BadUuid(String),
    #[error("invalid actor id: {0}")]
    BadActor(String),
    #[error("invalid surface id: {0}")]
    BadSurface(String),
    #[error("invalid git object id: {0}")]
    BadOid(String),
}

/// Errors for illegal DoorState transitions (§8: typed errors, never silent
/// coercion).
#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal transition from {from} via {event}")]
pub struct TransitionError {
    pub from: crate::DoorState,
    pub event: crate::DoorEvent,
}

/// Top-level domain error type for Phase I.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Id(#[from] IdError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
}
