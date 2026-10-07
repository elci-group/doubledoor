//! Domain types and the DoorSession state machine.
//!
//! This crate owns the concurrency model's vocabulary: session identity,
//! actor identity, mutation surfaces, canonical state, and the explicit
//! DoorState lifecycle. It has no I/O — persistence, Git, and policy live
//! in their own crates (§6 of the ELCI Technical Directive).

#![forbid(unsafe_code)]

mod assumption;
mod decision;
mod error;
pub mod evidence;
mod id;
mod notice;
mod session;
mod state;
mod surface;

pub use assumption::AssumptionViolation;
pub use decision::GhostDecision;
pub use error::{CoreError, IdError, TransitionError};
pub use id::{ActorId, CandidateId, DoorSessionId, GhostDiffId, GitOid, LeaseId, SurfaceId};
pub use notice::{ContentionNotice, IntentDescriptor};
pub use session::{
    AutoAcceptLevel, CanonicalState, DoorSession, GhostCursor, PolicySnapshot, WorkspaceRef,
};
pub use state::{DoorEvent, DoorState};
pub use surface::SurfaceSet;
