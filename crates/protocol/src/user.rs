//! The principal that owns a session.
//!
//! Lives in `protocol` rather than in `persistence` because `SessionState` carries it, and
//! `SessionState` is serialized into a `PromoteBundle` and shipped between machines. Slice 1b
//! additionally puts it on the wire in `Credentials` and `LoggedIn`.

use serde::{Deserialize, Serialize};

/// The reserved principal that owns every session until slice 1b introduces real identities.
const LOCAL: &str = "local";

/// Maximum length of a `UserId`, in bytes.
const MAX_LEN: usize = 64;

/// A principal's stable identifier.
///
/// Validated on construction **and on deserialization**: 1–64 characters of `[a-z0-9._-]`. That
/// charset keeps it safe as a sqlite key, as an `otpauth://` URI label (slice 1b), and in a log
/// line without escaping.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserId(String);

impl UserId {
    /// Validate and construct. See the type docs for the accepted charset.
    pub fn parse(s: &str) -> Result<Self, InvalidUserId> {
        if s.is_empty() || s.len() > MAX_LEN {
            return Err(InvalidUserId);
        }
        if !s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        }) {
            return Err(InvalidUserId);
        }
        Ok(Self(s.to_string()))
    }

    /// The reserved principal that owns sessions created by the offline CLI path and by every
    /// pre-identity `otto serve` connection. Never enrollable — slice 1b's `otto auth enroll`
    /// refuses it.
    pub fn local() -> Self {
        Self(LOCAL.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<UserId> for String {
    fn from(u: UserId) -> Self {
        u.0
    }
}

impl TryFrom<String> for UserId {
    type Error = InvalidUserId;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

/// A `UserId` that failed validation. Carries no detail: the rejected value is attacker-controlled
/// and echoing it into an error string invites log injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidUserId;

impl std::fmt::Display for InvalidUserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid user id: expected 1-64 characters of [a-z0-9._-]")
    }
}

impl std::error::Error for InvalidUserId {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_legal_charset() {
        for ok in ["alice", "a", "a.b_c-d", "user01", &"x".repeat(64)] {
            assert!(UserId::parse(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn parse_rejects_illegal_ids() {
        for bad in [
            "",              // empty
            &"x".repeat(65), // too long
            "Alice",         // uppercase
            "a b",           // space
            "../etc",        // path traversal characters
            "a'b",           // quote
            "a/b",           // separator
        ] {
            assert!(UserId::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn local_is_the_reserved_principal() {
        assert_eq!(UserId::local().as_str(), "local");
    }

    #[test]
    fn round_trips_as_a_bare_string() {
        let id = UserId::parse("alice").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"alice\"");
        assert_eq!(serde_json::from_str::<UserId>(&json).unwrap(), id);
    }

    /// The `try_from` guard is the point: a hostile `SessionState` arriving in a
    /// PromoteBundle must not be able to carry an owner that skipped validation.
    #[test]
    fn deserializing_an_illegal_id_fails() {
        assert!(serde_json::from_str::<UserId>("\"../etc/passwd\"").is_err());
        assert!(serde_json::from_str::<UserId>("\"\"").is_err());
    }
}
