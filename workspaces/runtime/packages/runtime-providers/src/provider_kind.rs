//! Single owner of provider identity.
//!
//! Stored account and agent records name their provider with [`ProviderKind::key`].
//! Python-written Codex records omit the field; that legacy representation is read
//! as Codex here and nowhere else. Any other value is a visible error, never a
//! silent default. (State files are not rewritten at startup: the restart-parity
//! corpora require reopening a state to leave its rows byte-identical.)

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;

/// The provider that owns an account and every agent on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    Claude,
}

/// A record names a provider this runtime does not know.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown provider: {0:?}")]
pub struct UnknownProvider(pub Option<String>);

impl ProviderKind {
    pub const ALL: [Self; 2] = [Self::Codex, Self::Claude];

    /// Stored record value of the `provider` field.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// Short display name for notices and UI text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }

    /// Read the `provider` field of a stored account or agent record.
    pub fn of_record(record: &Value) -> Result<Self, UnknownProvider> {
        Self::of_field(record.get("provider"))
    }

    /// Record key of the stored provider, or `None` for an unknown provider.
    #[must_use]
    pub fn key_of(record: &Value) -> Option<&'static str> {
        Self::of_record(record).ok().map(Self::key)
    }

    /// Parse a stored `provider` field value; absent or null is the legacy Codex form.
    pub fn of_field(field: Option<&Value>) -> Result<Self, UnknownProvider> {
        match field {
            Some(Value::String(value)) => value.parse(),
            Some(Value::Null) | None => Ok(Self::Codex),
            Some(other) => Err(UnknownProvider(Some(other.to_string()))),
        }
    }
}

/// How the session numbers the requests it sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestIdScheme {
    /// Unique JSON numbers.
    Numeric,
    /// Strings `{prefix}:{32 hex digits}`, unique per session.
    PrefixedHex(&'static str),
}

/// Wire behaviour of a provider's JSON-RPC peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireProfile {
    pub ids: RequestIdScheme,
    /// Thread start/resume/fork/read, turn start and steer wait per thread.
    pub per_thread_serialized: bool,
    /// The peer accepts cancellation of a pending request.
    pub cancellable: bool,
}

impl ProviderKind {
    /// Wire behaviour of this provider's peer (the Codex app-server or the Claude bridge).
    #[must_use]
    pub const fn wire(self) -> WireProfile {
        match self {
            Self::Codex => WireProfile {
                ids: RequestIdScheme::Numeric,
                per_thread_serialized: false,
                cancellable: false,
            },
            Self::Claude => WireProfile {
                ids: RequestIdScheme::PrefixedHex("claude"),
                per_thread_serialized: true,
                cancellable: true,
            },
        }
    }
}

impl FromStr for ProviderKind {
    type Err = UnknownProvider;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            other => Err(UnknownProvider(Some(other.to_owned()))),
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderKind, UnknownProvider};
    use serde_json::json;

    #[test]
    fn parses_only_known_providers() {
        for kind in ProviderKind::ALL {
            assert_eq!(kind.key().parse::<ProviderKind>(), Ok(kind));
            assert_eq!(
                serde_json::to_value(kind).expect("serialize"),
                json!(kind.key())
            );
            assert_eq!(
                ProviderKind::of_record(&json!({"provider": kind.key()})),
                Ok(kind)
            );
        }
        assert_eq!(
            "gemini".parse::<ProviderKind>(),
            Err(UnknownProvider(Some("gemini".into())))
        );
        assert_eq!(ProviderKind::of_record(&json!({})), Ok(ProviderKind::Codex));
        assert_eq!(
            ProviderKind::of_record(&json!({"provider": null})),
            Ok(ProviderKind::Codex)
        );
        assert_eq!(
            ProviderKind::of_record(&json!({"provider": ""})),
            Err(UnknownProvider(Some(String::new())))
        );
        assert_eq!(
            ProviderKind::of_record(&json!({"provider": 1})),
            Err(UnknownProvider(Some("1".into())))
        );
    }
}
