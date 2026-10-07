use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Schema version of the current build. Bump `major` for breaking layout or
/// semantic changes; bump `minor` for backward-compatible additions (Phase II
/// adds fields to persisted records, all with serde defaults).
pub const CURRENT_SCHEMA: SchemaVersion = SchemaVersion { major: 1, minor: 2 };

/// A two-part schema version, `major.minor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SchemaVersion {
    pub major: u16,
    pub minor: u16,
}

impl std::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Errors arising from schema version validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SchemaVersionError {
    #[error("schema version {0} is newer than supported version {CURRENT_SCHEMA}")]
    TooNew(SchemaVersion),
    #[error("unsupported schema version {0}")]
    Unsupported(SchemaVersion),
}

impl SchemaVersion {
    /// Validate that `self` can be understood by this build.
    pub fn check_supported(self) -> Result<(), SchemaVersionError> {
        if self.major > CURRENT_SCHEMA.major {
            return Err(SchemaVersionError::TooNew(self));
        }
        // Major versions below 1 are not a defined state.
        if self.major == 0 {
            return Err(SchemaVersionError::Unsupported(self));
        }
        Ok(())
    }
}

/// A versioned wrapper around a persisted or transmitted payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub schema: SchemaVersion,
    pub payload: T,
}

impl<T> Envelope<T> {
    pub fn new(payload: T) -> Self {
        Envelope {
            schema: CURRENT_SCHEMA,
            payload,
        }
    }

    /// Wrap a payload while recording the schema it was written with.
    pub fn with_schema(schema: SchemaVersion, payload: T) -> Self {
        Envelope { schema, payload }
    }

    /// Validate the envelope's schema version against this build.
    pub fn check_schema(&self) -> Result<(), SchemaVersionError> {
        self.schema.check_supported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_schema_is_supported() {
        assert!(CURRENT_SCHEMA.check_supported().is_ok());
    }

    #[test]
    fn newer_major_is_rejected() {
        let v = SchemaVersion {
            major: CURRENT_SCHEMA.major + 1,
            minor: 0,
        };
        assert_eq!(v.check_supported(), Err(SchemaVersionError::TooNew(v)));
    }

    #[test]
    fn major_zero_is_rejected() {
        let v = SchemaVersion { major: 0, minor: 9 };
        assert_eq!(v.check_supported(), Err(SchemaVersionError::Unsupported(v)));
    }

    #[test]
    fn older_minor_within_major_is_supported() {
        let v = SchemaVersion {
            major: CURRENT_SCHEMA.major,
            minor: CURRENT_SCHEMA.minor.saturating_sub(1),
        };
        assert!(v.check_supported().is_ok());
    }

    #[test]
    fn envelope_serializes_schema_first() {
        let env = Envelope::new("payload".to_string());
        let json = serde_json::to_string(&env).unwrap();
        let round: Envelope<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(round, env);
        assert!(round.check_schema().is_ok());
    }

    #[test]
    fn envelope_rejects_future_schema_on_read() {
        let json = format!(
            r#"{{"schema":{{"major":{},"minor":0}},"payload":"x"}}"#,
            CURRENT_SCHEMA.major + 1
        );
        let parsed: Result<Envelope<String>, _> = serde_json::from_str(&json);
        // serde itself succeeds; the check must be explicit per §44.
        let env = parsed.unwrap();
        assert!(matches!(
            env.check_schema(),
            Err(SchemaVersionError::TooNew(_))
        ));
    }
}
