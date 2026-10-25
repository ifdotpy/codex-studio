//! Identity and correction policy shared by durable budget and analytics receipts.
use super::{UsageEvidence, UsageObservation, UsageParseError, UsageProvider, UsageScope};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Budget fault while at least one provider response has unresolved usage.
pub const MISSING_FINAL_USAGE: &str = "missingFinalUsage";
/// Conflict fault when a recorded response's immutable token tuple changes.
pub const RESPONSE_REPLAY_CONFLICT: &str = "responseReplayConflict";

/// Logical owner of usage, independent of capture time, account, turn and provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum UsageIdentity {
    /// A provider response (including a gap awaiting its final usage).
    Response { agent: String, response_id: String },
    /// A running counter, whose individual observations have separate receipts.
    Cumulative {
        agent: String,
        thread_id: Option<String>,
        scope: UsageScope,
    },
}
impl UsageIdentity {
    pub fn new(agent: &str, thread: Option<&str>, observation: &UsageObservation) -> Self {
        match observation {
            UsageObservation::Response { response_id, .. }
            | UsageObservation::Gap { response_id, .. } => Self::Response {
                agent: agent.into(),
                response_id: response_id.clone(),
            },
            UsageObservation::Cumulative { scope, .. } => Self::Cumulative {
                agent: agent.into(),
                thread_id: thread.map(str::to_owned),
                scope: *scope,
            },
        }
    }
}

/// Durable typed receipt. Analytics stores these fields alongside presentation metadata;
/// budget stores the same value alongside its accounting placement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageLedgerRow {
    /// Immutable logical usage identity; dimensionless.
    pub usage_identity: UsageIdentity,
    /// First-capture deduplication key; retained across restore and re-import.
    pub receipt_key: String,
    /// Provider-neutral evidence; token fields have units of tokens.
    pub studio_usage: UsageObservation,
}
impl UsageLedgerRow {
    /// Capture compatibility is confined here: old counter receipts hashed exact wire
    /// objects. The immutable logical identity always comes from the typed observation.
    pub fn capture(
        agent: &str,
        account: &str,
        thread: Option<&str>,
        turn: Option<&str>,
        params: &Value,
        observation: UsageObservation,
    ) -> Self {
        let usage_identity = UsageIdentity::new(agent, thread, &observation);
        let receipt = match &usage_identity {
            UsageIdentity::Response { response_id, .. } => json!([agent, "response", response_id]),
            UsageIdentity::Cumulative { .. } => {
                super::legacy_notice_identity(agent, account, turn, params)
                    .unwrap_or_else(|| json!([agent, account, "notice", turn, observation]))
            }
        };
        Self {
            usage_identity,
            receipt_key: receipt_hash(&receipt),
            studio_usage: observation,
        }
    }

    /// Read typed persisted rows directly; legacy decoding stays at its supported boundary.
    pub fn from_analytics(provider: UsageProvider, row: &Value) -> Result<Self, UsageParseError> {
        let agent = row["agentId"].as_str().unwrap_or_default();
        if row.get("usageIdentity").is_some() || row.get("receiptKey").is_some() {
            let ledger: Self = serde_json::from_value(row.clone())?;
            let identity =
                UsageIdentity::new(agent, row["threadId"].as_str(), &ledger.studio_usage);
            if ledger.usage_identity != identity || ledger.receipt_key.is_empty() {
                return Err(UsageParseError("persisted usage identity mismatch".into()));
            }
            return Ok(ledger);
        }
        let observation = super::persisted::parse_analytics_usage_row(provider, row)?;
        Ok(Self::capture(
            agent,
            row["accountKey"].as_str().unwrap_or("default"),
            row["threadId"].as_str(),
            row["turnId"].as_str(),
            &Value::Null,
            observation,
        ))
    }
}

/// Stable digest of a receipt's canonical JSON representation; dimensionless.
pub fn receipt_hash(value: &Value) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(
        crate::json_compat::python_json_with_separators(value, ",", ":").as_bytes(),
    ))
}

/// Whether provisional response evidence can be replaced by incoming final evidence.
pub fn supersedes(previous: UsageEvidence, incoming: UsageEvidence) -> bool {
    previous == UsageEvidence::Legacy && incoming != UsageEvidence::Legacy
}

/// A rejected replay, preserving both totals without inventing missing legacy categories.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageReplayConflict {
    pub identity: UsageIdentity,
    /// Retained billable total; tokens.
    pub retained_total_tokens: u64,
    /// Rejected billable total; tokens.
    pub incoming_total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_tokens: Option<super::UsageTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_tokens: Option<super::UsageTokens>,
}

/// Authoritative budget decision forwarded to analytics without interpreting usage again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum UsageAcceptance {
    Accepted,
    RejectedReplayConflict(UsageReplayConflict),
}

/// Result of applying new evidence to an existing logical usage identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageCorrection {
    Apply,
    /// Retain provisional response tokens while recording their unresolved gap.
    UnresolvedGap,
    Replay,
    ResponseReplayConflict,
}

/// The first final token tuple is immutable. Gaps and provisional responses may
/// be resolved by final evidence; later gaps cannot erase a recorded response.
pub fn correction(previous: &UsageObservation, incoming: &UsageObservation) -> UsageCorrection {
    match (previous, incoming) {
        (
            UsageObservation::Response {
                tokens: old,
                evidence,
                ..
            },
            UsageObservation::Response {
                tokens,
                evidence: next,
                ..
            },
        ) => {
            if supersedes(*evidence, *next) {
                UsageCorrection::Apply
            } else if old == tokens {
                UsageCorrection::Replay
            } else {
                UsageCorrection::ResponseReplayConflict
            }
        }
        (
            UsageObservation::Response {
                evidence: UsageEvidence::Legacy,
                ..
            },
            UsageObservation::Gap { .. },
        ) => UsageCorrection::UnresolvedGap,
        (UsageObservation::Response { .. }, UsageObservation::Gap { .. }) => {
            UsageCorrection::Replay
        }
        (UsageObservation::Gap { .. }, UsageObservation::Response { evidence, .. })
            if *evidence != UsageEvidence::Legacy =>
        {
            UsageCorrection::Apply
        }
        (UsageObservation::Gap { .. }, _) => UsageCorrection::Replay,
        (old, next) if old == next => UsageCorrection::Replay,
        _ => UsageCorrection::Apply,
    }
}
