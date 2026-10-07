//! The agent protocol (§31, §50 Phase VI): `doubledoor.protocol/v1`.
//!
//! Line-delimited JSON over a Unix domain socket (`doubledoor serve`) or
//! stdio (`doubledoor rpc`); transport-neutral by construction (§13) — the
//! same values would travel over HTTP or WebSocket unchanged.
//!
//! - client → server: one [`Request`] per line;
//! - server → client: one [`Response`] per request (same `id`), plus
//!   [`Notification`]s for active subscriptions;
//! - the first request on a connection must be `hello` (capability
//!   negotiation, [`Hello`] → [`HelloResult`]); `describe` is always allowed.
//!
//! Errors are structured ([`ErrorObject`]): the stable §30 exit code, a
//! stable `kind`, whether retrying can help, and a message for humans that
//! agents never need to parse.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::SequencedEvent;

/// The protocol this build speaks.
pub const PROTOCOL: &str = "doubledoor.protocol/v1";
/// Schema tag of the machine-readable session report.
pub const REPORT_SCHEMA: &str = "doubledoor.report/v1";
/// Largest accepted request line.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Echoed back on the response; any JSON value.
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: Value,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: ErrorObject) -> Self {
        Response {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// The stable §30 exit code of the equivalent CLI failure.
    pub code: u8,
    /// Stable machine name (`not_found`, `invalid_transition`, …).
    pub kind: String,
    /// Retrying the same request later can succeed (busy, stale, lock).
    pub retryable: bool,
    /// For humans; agents key off `code`/`kind`.
    pub message: String,
}

impl ErrorObject {
    pub fn from_code(code: u8, message: impl Into<String>) -> Self {
        let (kind, retryable) = exit_kind(code);
        ErrorObject {
            code,
            kind: kind.to_string(),
            retryable,
            message: message.into(),
        }
    }

    /// Protocol-level errors that have no CLI counterpart (bad JSON, unknown
    /// method, missing handshake) are usage errors (code 1) with their own
    /// kind.
    pub fn protocol(kind: &str, message: impl Into<String>) -> Self {
        ErrorObject {
            code: 1,
            kind: kind.to_string(),
            retryable: false,
            message: message.into(),
        }
    }
}

/// A server push for a subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// Always `"event"` in v1.
    pub notification: String,
    pub subscription: u64,
    pub event: SequencedEvent,
}

/// One row of the stable exit-code contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitCode {
    pub code: u8,
    pub kind: String,
    pub retryable: bool,
    pub meaning: String,
}

/// The §30 exit-code table: `(code, kind, retryable, meaning)`.
pub const EXIT_CODES: &[(u8, &str, bool, &str)] = &[
    (0, "ok", false, "success"),
    (
        1,
        "usage",
        false,
        "bad invocation or parameters, or no repository",
    ),
    (2, "policy_denied", false, "the policy denied the request"),
    (
        3,
        "contention_without_notice",
        false,
        "contended request without a valid Locksmith notice",
    ),
    (4, "isolation_failed", false, "workspace isolation failed"),
    (
        5,
        "not_found",
        false,
        "session, ghost, or candidate not found",
    ),
    (
        6,
        "invalid_transition",
        true,
        "not legal in the current state (or the session is busy)",
    ),
    (
        7,
        "corrupt_metadata",
        false,
        "metadata or journal corrupt or schema-incompatible",
    ),
    (8, "internal", false, "internal error"),
    (9, "not_implemented", false, "declared for a later phase"),
    (
        10,
        "conflict",
        false,
        "change cannot be applied cleanly; nothing changed",
    ),
    (
        11,
        "lock_not_permitted",
        true,
        "the Locksmith lease does not permit the mutation",
    ),
    (
        12,
        "canonical_advanced",
        true,
        "canonical advanced past the candidate; it is stale",
    ),
];

pub fn exit_kind(code: u8) -> (&'static str, bool) {
    EXIT_CODES
        .iter()
        .find(|(c, ..)| *c == code)
        .map(|(_, k, r, _)| (*k, *r))
        .unwrap_or(("internal", false))
}

pub fn exit_codes() -> Vec<ExitCode> {
    EXIT_CODES
        .iter()
        .map(|(code, kind, retryable, meaning)| ExitCode {
            code: *code,
            kind: kind.to_string(),
            retryable: *retryable,
            meaning: meaning.to_string(),
        })
        .collect()
}

/// `hello` parameters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// Versions the client speaks, most preferred first.
    #[serde(default)]
    pub protocol_versions: Vec<String>,
    /// Capabilities the client would like to use.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Capabilities without which the client cannot work: negotiation fails
    /// (`capability_unavailable`) if any is not granted.
    #[serde(default)]
    pub require: Vec<String>,
    /// Free-form client identification (`{"name": "codex", "version": …}`).
    #[serde(default)]
    pub agent: Option<Value>,
}

/// One parameter of a method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    /// `string`, `string[]`, `boolean`, `integer`, `object`.
    #[serde(rename = "type")]
    pub ty: String,
    pub required: bool,
    pub doc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodSpec {
    pub name: String,
    pub doc: String,
    pub params: Vec<ParamSpec>,
    /// Shape of `result` (a schema tag or a short structural description).
    pub result: String,
    /// The method starts a notification stream.
    pub streams: bool,
}

/// `hello` result: the negotiated contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: String,
    pub server: Value,
    /// Schema versions of everything a client may receive.
    pub schemas: Value,
    pub methods: Vec<MethodSpec>,
    /// Every capability this server offers, with live detail.
    pub capabilities: Value,
    /// Requested ∩ offered.
    pub granted: Vec<String>,
    /// Requested but not offered.
    pub unsupported: Vec<String>,
    pub exit_codes: Vec<ExitCode>,
    pub limits: Value,
}

/// A machine-readable next step (§50): the method to call and its params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextAction {
    /// A protocol method name (`accept`, `reconcile`, `promote`, …) or
    /// `wait` while the session is busy.
    pub action: String,
    pub params: Value,
    /// Params the caller must add before calling (e.g. `lease`).
    #[serde(default)]
    pub requires: Vec<String>,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_round_trip() {
        let r: Request = serde_json::from_str(r#"{"id": 7, "method": "status"}"#).unwrap();
        assert_eq!(r.method, "status");
        assert!(r.params.is_null());
        let ok = serde_json::to_value(Response::ok(Value::from(7), Value::from("x"))).unwrap();
        assert_eq!(ok["ok"], true);
        assert!(ok.get("error").is_none());
        let err = serde_json::to_value(Response::err(
            Value::Null,
            ErrorObject::from_code(12, "stale"),
        ))
        .unwrap();
        assert_eq!(err["error"]["kind"], "canonical_advanced");
        assert_eq!(err["error"]["retryable"], true);
    }

    #[test]
    fn exit_table_is_complete_and_unique() {
        let codes: Vec<u8> = EXIT_CODES.iter().map(|r| r.0).collect();
        assert_eq!(codes, (0..=12).collect::<Vec<u8>>());
        assert_eq!(exit_kind(5).0, "not_found");
        assert_eq!(exit_kind(99).0, "internal");
    }
}
