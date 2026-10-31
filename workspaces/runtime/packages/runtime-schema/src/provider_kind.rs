//! Single owner of provider identity.
//!
//! Stored account and agent records name their provider with [`ProviderKind::key`].
//! Ordinary readers require an explicit known identity. Historical missing/null
//! identities are handled only by the dated 2026-10-26 startup migration.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;

/// The provider that owns an account and every agent on it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    Claude,
}

/// A record names a provider this runtime does not know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownProvider(pub Option<String>);

impl fmt::Display for UnknownProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown provider: {:?}", self.0)
    }
}
impl std::error::Error for UnknownProvider {}

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
        let object = record
            .as_object()
            .ok_or_else(|| UnknownProvider(Some(record.to_string())))?;
        Self::of_field(object.get("provider"))
    }

    /// Record key of the stored provider, or `None` for an unknown provider.
    #[must_use]
    pub fn key_of(record: &Value) -> Option<&'static str> {
        Self::of_record(record).ok().map(Self::key)
    }

    /// Parse an explicit stored `provider` field value.
    pub fn of_field(field: Option<&Value>) -> Result<Self, UnknownProvider> {
        match field {
            Some(Value::String(value)) => value.parse(),
            None => Err(UnknownProvider(None)),
            Some(other) => Err(UnknownProvider(Some(other.to_string()))),
        }
    }
}

/// A control or behavior that a provider adapter supports. The UI shows only the
/// controls of the adapter's list; a missing entry hides the control.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    /// Input added to a running turn.
    Steer,
    /// Restore the thread to an earlier point.
    Rewind,
    /// Start a new thread from an earlier point.
    Fork,
    /// Change the model of a live thread.
    ModelChange,
    /// Set the reasoning effort.
    Effort,
    /// Compact the thread context.
    Compaction,
    /// Ask the user before a tool call.
    Approvals,
}

impl ProviderCapability {
    pub const ALL: [Self; 7] = [
        Self::Steer,
        Self::Rewind,
        Self::Fork,
        Self::ModelChange,
        Self::Effort,
        Self::Compaction,
        Self::Approvals,
    ];

    /// Wire and generated-contract spelling.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Rewind => "rewind",
            Self::Fork => "fork",
            Self::ModelChange => "model_change",
            Self::Effort => "effort",
            Self::Compaction => "compaction",
            Self::Approvals => "approvals",
        }
    }
}

impl ProviderKind {
    /// The capability list of this provider's adapter. The exhaustive match
    /// forces a new provider to declare its list.
    #[must_use]
    pub const fn capabilities(self) -> &'static [ProviderCapability] {
        use ProviderCapability::{Approvals, Compaction, Effort, Fork, ModelChange, Rewind, Steer};
        match self {
            // `edit(turn)` serves every provider, so both support rewind.
            Self::Codex | Self::Claude => &[
                Steer,
                Rewind,
                Fork,
                ModelChange,
                Effort,
                Compaction,
                Approvals,
            ],
        }
    }

    #[must_use]
    pub fn supports(self, capability: ProviderCapability) -> bool {
        self.capabilities().contains(&capability)
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

#[deny(clippy::disallowed_types)]
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
    use super::{ProviderCapability, ProviderKind, UnknownProvider};
    use serde_json::json;

    #[test]
    fn capability_lists_are_declared_per_provider() {
        use ProviderCapability::*;
        for kind in ProviderKind::ALL {
            assert_eq!(
                kind.capabilities(),
                [
                    Steer,
                    Rewind,
                    Fork,
                    ModelChange,
                    Effort,
                    Compaction,
                    Approvals
                ],
                "{kind}"
            );
            assert!(kind.supports(Rewind), "{kind}");
        }
        for kind in ProviderKind::ALL {
            let list = kind.capabilities();
            assert!(
                list.iter().all(|c| ProviderCapability::ALL.contains(c)),
                "{kind}"
            );
        }
    }

    #[test]
    fn capability_keys_match_serde() {
        for capability in ProviderCapability::ALL {
            assert_eq!(
                serde_json::to_value(capability).expect("serialize"),
                json!(capability.key())
            );
        }
    }

    #[test]
    fn generated_provider_schema_preserves_wire_identity() {
        let schema = schemars::schema_for!(ProviderKind).to_value();
        assert_eq!(
            schema["enum"],
            json!(ProviderKind::ALL.map(ProviderKind::key))
        );
        assert_eq!(
            ProviderKind::ALL.map(ProviderKind::label),
            ["Codex", "Claude"]
        );
    }

    #[test]
    fn malformed_records_never_acquire_legacy_codex_identity() {
        for record in [
            json!(null),
            json!([]),
            json!("codex"),
            json!(1),
            json!(true),
        ] {
            assert!(ProviderKind::of_record(&record).is_err(), "{record}");
            assert_eq!(ProviderKind::key_of(&record), None, "{record}");
        }
        for field in [
            json!("gemini"),
            json!(""),
            json!("Codex"),
            json!(1),
            json!(true),
            json!([]),
            json!({}),
        ] {
            assert!(ProviderKind::of_field(Some(&field)).is_err(), "{field}");
            assert!(serde_json::from_value::<ProviderKind>(field).is_err());
        }
        assert!(serde_json::from_value::<ProviderKind>(json!(null)).is_err());
    }

    #[test]
    fn ordinary_readers_require_explicit_provider_identity() {
        for record in [json!({}), json!({"provider":null})] {
            assert!(ProviderKind::of_record(&record).is_err(), "{record}");
            assert_eq!(ProviderKind::key_of(&record), None);
        }
        assert!(ProviderKind::of_field(None).is_err());
        assert!(ProviderKind::of_field(Some(&json!(null))).is_err());
    }

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
