//! Ghost observation for Doubledoor (§46, Phase II "Ghost Observation").
//!
//! When external actors commit to the canonical repository while a DoorSession
//! speculates on top of it, each such commit becomes a *ghost*: a captured,
//! classified, and persisted observation of world-state change the session's
//! actor has not yet assessed. This crate owns:
//!
//! - [`parse_unified_diff`] — structural parsing of commit diffs (hunk
//!   headers only, capped at [`MAX_DIFF_BYTES`] per §27);
//! - [`assess`] — Phase III per-surface assessment (file, Rust symbol, Cargo
//!   layers) with assumption invalidation;
//! - [`classify`] / [`classify_observation`] — the §12 file-level classifier
//!   (W_m∩W_s → WriteOverlap, W_m∩R_s → ReadRelevant, W_m∩D_s →
//!   DependencyRelevant, none → Independent, analysis failure → Unknown);
//! - [`GhostDiff`] — the persisted record of one observation;
//! - [`GhostStore`] — per-session atomic persistence under
//!   `.doubledoor/sessions/<id>/ghosts/`;
//! - [`generate`] — the observation walk from a session's ghost cursor to
//!   `HEAD`;
//! - [`accept_into_workspace`] — ACCEPT (§14/§15): the explicit,
//!   optionally surface-selective import of a ghost into a workspace.
//!
//! Every Git access goes through the `doubledoor-git` choke point; this crate
//! never spawns processes itself.

#![forbid(unsafe_code)]

mod apply;
mod assess;
mod classify;
mod diff;
mod error;
mod generate;
mod ghost;
mod store;

pub use apply::{accept_into_workspace, AcceptOutcome};
pub use assess::{assess, rank, Assessment, SurfaceAssessment};
pub use classify::{classify, classify_observation, GhostClassification};
pub use diff::{
    parse_unified_diff, parse_unified_diff_limited, ChangeKind, DiffParseError, FileChange, Hunk,
    TextualDelta, MAX_DIFF_BYTES,
};
pub use error::GhostError;
pub use generate::{generate, generate_with, GhostObservation};
pub use ghost::GhostDiff;
pub use store::GhostStore;
