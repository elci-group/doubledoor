//! Schema-versioned envelopes and wire/CLI representations.
//!
//! Every persistent or protocol structure travels inside an [`Envelope`] whose
//! [`SchemaVersion`] is checked on deserialization. Unknown or newer major
//! versions are rejected explicitly — never silently assumed compatible
//! (ELCI Technical Directive §44).

#![forbid(unsafe_code)]

mod envelope;
mod event;
pub mod rpc;
mod view;

pub use envelope::{Envelope, SchemaVersion, SchemaVersionError, CURRENT_SCHEMA};
pub use event::{SequencedEvent, StreamEvent};
pub use view::{
    AssessmentView, AssumptionView, DecisionView, DoctorCheck, DoctorReport, GhostView,
    RecoveryAction, RecoveryReport, SessionObservation, SessionSummary, StatusReport, SurfacesView,
};
