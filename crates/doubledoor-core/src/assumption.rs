//! Cross-session assumption tracking (§12).
//!
//! Phase II's file-level ghost classifier never detects assumption violations —
//! that requires the semantic analysis of Phase III — but the type is fixed
//! now so persisted GhostDiff records carry a stable schema from day one.
//! Every GhostDiff written in Phase II has an empty
//! `invalidated_assumptions` vector.

use serde::{Deserialize, Serialize};

use crate::SurfaceId;

/// A cross-session assumption the session relied on and a ghost commit was
/// observed to violate (§12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssumptionViolation {
    /// The surface on which the violated assumption rests.
    pub surface: SurfaceId,
    /// Human-readable description of what was assumed and how it broke.
    pub description: String,
}

impl AssumptionViolation {
    pub fn new(surface: SurfaceId, description: impl Into<String>) -> Self {
        AssumptionViolation {
            surface,
            description: description.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn violation_roundtrips_through_serialization() {
        let v = AssumptionViolation::new(
            SurfaceId::new("file:src/router.rs").unwrap(),
            "assumed handler ordering from session dd_other",
        );
        let json = serde_json::to_string(&v).unwrap();
        let back: AssumptionViolation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }
}
