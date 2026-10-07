//! Reconciliation (§16, §48 Phase IV): candidate construction, the conflict
//! graph, item-level merging, symbol-level selective import, the §52
//! compatibility report, and compare-and-swap promotion.
//!
//! - [`units`] / [`merge`] — Rust files split into items (with exact
//!   spans) and merged three-way; contested items are never blended;
//! - [`candidate`] — snapshot the session, merge it with canonical, and
//!   write a candidate commit on top of canonical `HEAD`, privately;
//! - [`import`] — `accept --symbol`: bring chosen items of a ghost into a
//!   workspace;
//! - [`compat`] — map compiler diagnostics onto the session's retained code;
//! - [`promote`] — fast-forward canonical from exactly the candidate's
//!   target, and classify interrupted promotions.
//!
//! Canonical state is never touched before promotion (§16); every Git
//! access goes through the `doubledoor-git` choke point.

#![forbid(unsafe_code)]

pub mod candidate;
pub mod compat;
mod error;
pub mod import;
pub mod merge;
pub mod promote;
pub mod store;
pub mod units;

pub use candidate::{build, BuildInput, Candidate, CandidateStatus};
pub use error::ReconcileError;
