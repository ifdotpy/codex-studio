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

/// Maximum analytics source rows examined to establish old gap identities; rows.
pub const LEGACY_GAP_COVERAGE_ROWS: usize = 256;

/// Whether the budget knows every unresolved response identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum GapCoverage {
    #[default]
    Complete,
    LegacyUnknown,
}

/// Immutable response receipt key, shared by responses and their gaps.
pub fn response_receipt_key(agent: &str, response: &str) -> String {
    receipt_hash(&json!([agent, "response", response]))
}

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
    pub receipt_key: Option<String>,
    /// Provider-neutral evidence; token fields have units of tokens.
    pub studio_usage: UsageObservation,
}
impl UsageLedgerRow {
    /// New receipts derive only from typed accounting facts; legacy spellings
    /// remain aliases and never alter response identity.
    pub fn capture(agent: &str, thread: Option<&str>, observation: UsageObservation) -> Self {
        let usage_identity = UsageIdentity::new(agent, thread, &observation);
        let mut row = Self {
            usage_identity,
            receipt_key: None,
            studio_usage: observation,
        };
        row.receipt_key = Some(match &row.usage_identity {
            UsageIdentity::Response { response_id, .. } => response_receipt_key(agent, response_id),
            UsageIdentity::Cumulative { .. } => row.observation_key(),
        });
        row
    }

    /// Identity of the typed counter evidence, independent of erased legacy wire fields.
    pub fn observation_key(&self) -> String {
        let accounting = match &self.studio_usage {
            UsageObservation::Cumulative {
                turn_id,
                total,
                last,
                ..
            } => json!([turn_id, total, last]),
            UsageObservation::Response { tokens, .. } => json!(tokens),
            UsageObservation::Gap { .. } => Value::Null,
        };
        receipt_hash(&json!([self.usage_identity, accounting]))
    }

    /// Whether an analytics record predates persisted ledger identity metadata.
    pub fn is_pre_ledger_row(row: &Value) -> bool {
        row.get("usageIdentity").is_none() && row.get("receiptKey").is_none()
    }

    /// Read typed persisted rows directly; legacy decoding stays at its supported boundary.
    pub fn from_analytics(provider: UsageProvider, row: &Value) -> Result<Self, UsageParseError> {
        let agent = row["agentId"].as_str().unwrap_or_default();
        if row.get("usageIdentity").is_some() || row.get("receiptKey").is_some() {
            let ledger: Self = serde_json::from_value(row.clone())?;
            let identity =
                UsageIdentity::new(agent, row["threadId"].as_str(), &ledger.studio_usage);
            if ledger.usage_identity != identity
                || ledger.receipt_key.as_deref().is_none_or(str::is_empty)
            {
                return Err(UsageParseError("persisted usage identity mismatch".into()));
            }
            return Ok(ledger);
        }
        let observation = super::persisted::parse_analytics_usage_row(provider, row)?;
        let usage_identity = UsageIdentity::new(agent, row["threadId"].as_str(), &observation);
        let receipt_key = match &usage_identity {
            UsageIdentity::Response { response_id, .. } => {
                Some(receipt_hash(&json!([agent, "response", response_id])))
            }
            // A counter's original wire receipt may contain fields no longer in the row.
            // Only the budget receipt table can establish a legacy key unambiguously.
            UsageIdentity::Cumulative { .. } => None,
        };
        Ok(Self {
            usage_identity,
            receipt_key,
            studio_usage: observation,
        })
    }
}

fn legacy_counter_preimage(
    agent: &str,
    account: &str,
    turn: Option<&str>,
    params: &Value,
    observation: &UsageObservation,
) -> Value {
    super::legacy_notice_identity(agent, account, turn, params)
        .unwrap_or_else(|| json!([agent, account, "notice", turn, observation]))
}

/// Exact pre-v5 receipt aliases, checked against durable receipts before reuse.
pub fn legacy_receipt_keys(
    agent: &str,
    account: &str,
    turn: Option<&str>,
    params: &Value,
    observation: &UsageObservation,
) -> Vec<String> {
    if !matches!(observation, UsageObservation::Cumulative { .. }) {
        return Vec::new();
    }
    vec![receipt_hash(&legacy_counter_preimage(
        agent,
        account,
        turn,
        params,
        observation,
    ))]
}

/// Stable identity for the total-only compatibility reader; it never invents categories.
pub fn legacy_counter_observation_key(
    agent: &str,
    thread: Option<&str>,
    legacy: &super::LegacyUsageTotals,
) -> String {
    let identity = UsageIdentity::Cumulative {
        agent: agent.into(),
        thread_id: thread.map(str::to_owned),
        scope: legacy.scope,
    };
    receipt_hash(&json!([
        identity,
        [
            legacy.turn_id,
            legacy.total_tokens,
            legacy.last_total_tokens
        ]
    ]))
}

/// Whether accounting evidence belongs to a migrated ledger's legacy era.
/// Epoch and observation time are server/provider Unix seconds respectively.
pub fn legacy_era(ledger_since: Option<f64>, at: f64, estimated: bool) -> bool {
    ledger_since.is_some_and(|since| estimated || at < since)
}

/// Recover supported old counter spellings from persisted typed facts, never by
/// reparsing a fabricated notification. Unknown wire extensions remain unknown.
pub fn legacy_analytics_receipt_keys(row: &Value, observation: &UsageObservation) -> Vec<String> {
    let UsageObservation::Cumulative {
        turn_id,
        total,
        last,
        ..
    } = observation
    else {
        return Vec::new();
    };
    let agent = row["agentId"].as_str().unwrap_or_default();
    let account = row["accountKey"].as_str().unwrap_or("default");
    let native = |tokens: &super::UsageTokens| json!({"inputTokens":tokens.uncached_input.saturating_add(tokens.cache_read_input).saturating_add(tokens.cache_write_input),"cachedInputTokens":tokens.cache_read_input,"outputTokens":tokens.output,"reasoningOutputTokens":tokens.reasoning_output,"totalTokens":tokens.total()});
    let mut keys = Vec::new();
    for turn in [row["turnId"].as_str(), turn_id.as_deref()] {
        for preimage in [
            json!([agent, account, "notice", turn, row["total"], row["last"]]),
            json!([
                agent,
                account,
                "notice",
                turn,
                native(total),
                last.as_ref().map(native)
            ]),
            json!([agent, account, "notice", turn, observation]),
        ] {
            let key = receipt_hash(&preimage);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

/// Fault when an old counter's original debit cannot be identified safely.
pub const UNKNOWN_USAGE_RECEIPT: &str = "unknownUsageReceipt";

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
            UsageObservation::Cumulative {
                scope: old_scope,
                turn_id: old_turn,
                total: old_total,
                last: old_last,
                ..
            },
            UsageObservation::Cumulative {
                scope,
                turn_id,
                total,
                last,
                ..
            },
        ) if old_scope == scope
            && old_turn == turn_id
            && old_total == total
            && old_last == last =>
        {
            UsageCorrection::Replay
        }
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
