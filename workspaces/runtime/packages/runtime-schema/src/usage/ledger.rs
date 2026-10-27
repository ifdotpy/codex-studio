//! Identity and correction policy shared by durable budget and analytics receipts.
use super::{UsageEvidence, UsageObservation, UsageParseError, UsageScope};
use crate::provider_kind::ProviderKind;
use crate::queue_command::Field;
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

/// Maximum retained diagnostic entries per kind in one budget ledger; entries.
pub const LEDGER_DIAGNOSTIC_ITEMS: usize = 64;
/// Maximum serialized JSON storage per diagnostic kind; UTF-8 bytes.
pub const LEDGER_DIAGNOSTIC_BYTES: usize = 16_384;
/// Maximum native thread identifier length at capture ingress; Unicode characters.
pub const MAX_NATIVE_THREAD_ID_CHARS: usize = 128;
/// A durable replay receipt with no known debit; dimensionless row kind.
pub const UNKNOWN_USAGE_KIND: &str = "unknown";

/// The wire preserves missing (v4), null (v5-created), and migration Unix seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum LedgerEpoch {
    #[default]
    Legacy,
    Current,
    Since(f64),
}
impl LedgerEpoch {
    pub fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy)
    }
}
impl Serialize for LedgerEpoch {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Legacy => Err(serde::ser::Error::custom(
                "legacy ledger epoch must be omitted",
            )),
            Self::Current => serializer.serialize_none(),
            Self::Since(epoch) if epoch.is_finite() && *epoch >= 0. => {
                serializer.serialize_f64(*epoch)
            }
            Self::Since(_) => Err(serde::ser::Error::custom("invalid ledger epoch")),
        }
    }
}
impl<'de> Deserialize<'de> for LedgerEpoch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Option::<f64>::deserialize(deserializer)? {
            None => Ok(Self::Current),
            Some(epoch) if epoch.is_finite() && epoch >= 0. => Ok(Self::Since(epoch)),
            Some(_) => Err(serde::de::Error::custom("invalid ledger epoch")),
        }
    }
}

/// Sticky loss of complete diagnostic coverage, independently for each list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct LedgerDiagnosticOverflow {
    pub ambiguous_notices: bool,
    pub faults: bool,
    pub unresolved_gaps: bool,
    pub replay_conflicts: bool,
}

/// Retain a bounded prefix without losing whether evidence was dropped.
pub fn bound_diagnostics<T: Serialize>(values: &mut Vec<T>) -> bool {
    let mut bytes = 2;
    let mut keep = 0;
    for value in values.iter().take(LEDGER_DIAGNOSTIC_ITEMS) {
        let Ok(encoded) = serde_json::to_vec(value) else {
            break;
        };
        let next = bytes + encoded.len() + usize::from(keep != 0);
        if next > LEDGER_DIAGNOSTIC_BYTES {
            break;
        }
        bytes = next;
        keep += 1;
    }
    let overflow = values.len() > keep;
    values.truncate(keep);
    overflow
}

/// Generated persisted-budget constraints share the retention owner's item bound.
/// Older rows lack the bounds version and remain readable for lazy normalization.
pub fn budget_state_schema() -> Value {
    let mut schema: Value = serde_json::from_str(include_str!(
        "../schemas/persisted-budget-state.schema.json"
    ))
    .expect("budget schema");
    schema["properties"]
        .as_object_mut()
        .expect("properties")
        .remove("diagnosticBoundsVersion");
    schema["properties"]
        .as_object_mut()
        .expect("properties")
        .remove("unknownReceipts");
    schema["properties"]["unknownUsage"] = json!({"type":"boolean"});
    schema.as_object_mut().expect("schema").remove("allOf");
    schema["properties"]["diagnosticOverflow"] =
        schemars::schema_for!(LedgerDiagnosticOverflow).to_value();
    for key in ["ambiguousNotices", "faults", "unresolvedGaps"] {
        schema["properties"][key] = json!({"type":"array","items":{"type":"string"}});
    }
    let mut conflict = schemars::schema_for!(UsageReplayConflict).to_value();
    if let Some(Value::Object(definitions)) = conflict
        .as_object_mut()
        .expect("schema object")
        .remove("$defs")
    {
        schema["$defs"]
            .as_object_mut()
            .expect("schema definitions")
            .extend(definitions);
    }
    conflict
        .as_object_mut()
        .expect("schema object")
        .remove("$schema");
    schema["properties"]["replayConflicts"] = json!({"type":"array", "items":conflict});
    schema
}

/// Strict output constraints; legacy reads remain available for bounded normalization.
pub fn budget_state_write_schema() -> Value {
    let mut schema = budget_state_schema();
    for key in [
        "ambiguousNotices",
        "faults",
        "unresolvedGaps",
        "replayConflicts",
    ] {
        schema["properties"][key]["maxItems"] = json!(LEDGER_DIAGNOSTIC_ITEMS);
    }
    schema
}

/// An unknown receipt is immutable because its primary key is the accounting facts.
pub fn receipt_replay(kind: &str) -> Option<UsageCorrection> {
    (kind == UNKNOWN_USAGE_KIND).then_some(UsageCorrection::Replay)
}

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
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageLedgerRow {
    /// Immutable logical usage identity; dimensionless.
    usage_identity: UsageIdentity,
    /// First-capture deduplication key; retained across restore and re-import.
    receipt_key: Option<String>,
    /// Provider-neutral evidence; token fields have units of tokens.
    studio_usage: UsageObservation,
}
// Remote derive preserves the row's permissive presentation metadata boundary.
#[derive(Deserialize)]
#[serde(remote = "UsageLedgerRow", rename_all = "camelCase")]
struct UsageLedgerRowWire {
    usage_identity: UsageIdentity,
    receipt_key: Option<String>,
    studio_usage: UsageObservation,
}

/// Cost receipt projection: presentation metadata is skipped during deserialization.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsReceipt {
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    usage_identity: Field<UsageIdentity>,
    #[serde(default)]
    receipt_key: Field<String>,
    studio_usage: UsageObservation,
}
impl<'de> Deserialize<'de> for UsageLedgerRow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let row = UsageLedgerRowWire::deserialize(deserializer)?;
        Self::new(row.usage_identity, row.receipt_key, row.studio_usage)
            .map_err(serde::de::Error::custom)
    }
}
/// Accounting identity and evidence cannot be independently replaced by callers.
/// ```compile_fail
/// use studio_runtime_schema::usage::ledger::{UsageIdentity, UsageLedgerRow};
/// fn replace_identity(mut row: UsageLedgerRow, identity: UsageIdentity) {
///     row.usage_identity = identity;
/// }
/// ```
impl UsageLedgerRow {
    /// Check the identity against its evidence; receipt aliases remain opaque.
    pub fn new(
        usage_identity: UsageIdentity,
        receipt_key: Option<String>,
        studio_usage: UsageObservation,
    ) -> Result<Self, UsageParseError> {
        let (agent, thread) = match &usage_identity {
            UsageIdentity::Response { agent, .. } => (agent.as_str(), None),
            UsageIdentity::Cumulative {
                agent, thread_id, ..
            } => (agent.as_str(), thread_id.as_deref()),
        };
        if usage_identity != UsageIdentity::new(agent, thread, &studio_usage) {
            return Err(UsageParseError("persisted usage identity mismatch".into()));
        }
        Ok(Self {
            usage_identity,
            receipt_key,
            studio_usage,
        })
    }
    pub fn usage_identity(&self) -> &UsageIdentity {
        &self.usage_identity
    }
    pub fn receipt_key(&self) -> Option<&str> {
        self.receipt_key.as_deref()
    }
    pub fn studio_usage(&self) -> &UsageObservation {
        &self.studio_usage
    }
    pub fn into_observation(self) -> UsageObservation {
        self.studio_usage
    }
    /// Preserve a proven historical receipt alias without changing accounting facts.
    pub fn set_receipt_key(&mut self, receipt_key: Option<String>) {
        self.receipt_key = receipt_key;
    }

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
    pub fn from_analytics(provider: ProviderKind, row: &Value) -> Result<Self, UsageParseError> {
        if row.get("studioUsage").is_some() || !Self::is_pre_ledger_row(row) {
            return Self::from_typed_analytics(row);
        }
        let observation = super::persisted::parse_analytics_usage_row(provider, row)?;
        let mut ledger = Self::capture(
            row["agentId"].as_str().unwrap_or_default(),
            row["threadId"].as_str(),
            observation,
        );
        // A pre-ledger counter's original wire receipt can only be proved by the budget table.
        if matches!(ledger.studio_usage, UsageObservation::Cumulative { .. }) {
            ledger.receipt_key = None;
        }
        Ok(ledger)
    }

    /// Read provider-neutral typed evidence while checking its persisted receipt identity.
    pub fn from_typed_analytics(row: &Value) -> Result<Self, UsageParseError> {
        let agent = row["agentId"].as_str().unwrap_or_default();
        if row.get("usageIdentity").is_some() || row.get("receiptKey").is_some() {
            let ledger = Self::deserialize(row)?;
            ledger.validate_analytics_owner(agent, row["threadId"].as_str())?;
            return Ok(ledger);
        }
        let observation = UsageObservation::deserialize(&row["studioUsage"])?;
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

    /// Decode a receipt once without building a JSON value or synthesizing an unused legacy key.
    pub fn observation_from_typed_analytics_str(
        raw: &str,
    ) -> Result<UsageObservation, UsageParseError> {
        let row: AnalyticsReceipt = serde_json::from_str(raw)?;
        if row.usage_identity.is_missing() && row.receipt_key.is_missing() {
            return Ok(row.studio_usage);
        }
        let Field::Value(identity) = row.usage_identity else {
            return Err(UsageParseError("persisted usage identity mismatch".into()));
        };
        let receipt = match row.receipt_key {
            Field::Value(key) => Some(key),
            Field::Missing | Field::Null => None,
        };
        let ledger = Self::new(identity, receipt, row.studio_usage)?;
        ledger.validate_analytics_owner(
            row.agent_id.as_deref().unwrap_or_default(),
            row.thread_id.as_deref(),
        )?;
        Ok(ledger.into_observation())
    }

    fn validate_analytics_owner(
        &self,
        agent: &str,
        thread: Option<&str>,
    ) -> Result<(), UsageParseError> {
        let matches_owner = match &self.usage_identity {
            UsageIdentity::Response { agent: owner, .. } => owner == agent,
            UsageIdentity::Cumulative {
                agent: owner,
                thread_id,
                ..
            } => owner == agent && thread_id.as_deref() == thread,
        };
        if !matches_owner || self.receipt_key.as_deref().is_none_or(str::is_empty) {
            return Err(UsageParseError("persisted usage identity mismatch".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod typed_analytics_tests {
    use super::*;

    #[test]
    fn ledger_deserialization_rejects_mismatched_observation_identity() {
        let observation: UsageObservation = serde_json::from_value(json!({"kind":"response","responseId":"response","model":"model","evidence":"live","tokens":{"uncachedInput":12,"cacheReadInput":3,"cacheWriteInput":2,"output":7}})).expect("usage");
        let ledger = UsageLedgerRow::capture("agent", Some("thread"), observation);
        let row = serde_json::to_value(&ledger).expect("ledger");
        assert_eq!(
            serde_json::from_value::<UsageLedgerRow>(row.clone()).expect("round trip"),
            ledger
        );
        for identity in [
            json!({"kind":"response","agent":"agent","responseId":"other"}),
            json!({"kind":"cumulative","agent":"agent","threadId":null,"scope":"codexThread"}),
        ] {
            let mut invalid = row.clone();
            invalid["usageIdentity"] = identity;
            assert!(serde_json::from_value::<UsageLedgerRow>(invalid).is_err());
        }
        let mut legacy = row;
        legacy["receiptKey"] = Value::Null;
        legacy["presentationMetadata"] = json!(true);
        assert!(
            serde_json::from_value::<UsageLedgerRow>(legacy.clone())
                .expect("nullable legacy receipt")
                .receipt_key()
                .is_none()
        );
        legacy.as_object_mut().expect("object").remove("receiptKey");
        assert!(
            serde_json::from_value::<UsageLedgerRow>(legacy)
                .expect("omitted legacy receipt")
                .receipt_key()
                .is_none()
        );
        let cumulative = UsageObservation::Cumulative {
            scope: UsageScope::CodexThread,
            turn_id: None,
            total: super::super::UsageTokens::default(),
            last: None,
            model_context_window: None,
            models: None,
        };
        let ledger = UsageLedgerRow::capture("agent", None, cumulative);
        let mut row = serde_json::to_value(&ledger).expect("counter ledger");
        row["usageIdentity"]["scope"] = json!("claudeQuery");
        assert!(serde_json::from_value::<UsageLedgerRow>(row).is_err());
    }

    #[test]
    fn provider_identity_typed_ledger_preserves_receipt_and_identity_checks() {
        let observation: UsageObservation = serde_json::from_value(json!({"kind":"response","responseId":"response","model":"model","evidence":"live","tokens":{"uncachedInput":12,"cacheReadInput":3,"cacheWriteInput":2,"output":7}})).expect("typed usage");
        let ledger = UsageLedgerRow::capture("agent", Some("thread"), observation);
        let mut row = serde_json::to_value(&ledger).expect("ledger");
        row["agentId"] = json!("agent");
        row["threadId"] = json!("thread");
        assert_eq!(
            UsageLedgerRow::from_typed_analytics(&row).expect("provider-neutral typed row"),
            ledger
        );
        for pointer in ["/receiptKey", "/usageIdentity/agent"] {
            let mut invalid = row.clone();
            *invalid.pointer_mut(pointer).expect("field") = json!("");
            assert!(UsageLedgerRow::from_typed_analytics(&invalid).is_err());
        }
        row["studioUsage"]["tokens"]["output"] = json!("invalid");
        assert!(UsageLedgerRow::from_typed_analytics(&row).is_err());

        let observation = UsageObservation::Cumulative {
            scope: UsageScope::CodexThread,
            turn_id: None,
            total: super::super::UsageTokens::default(),
            last: None,
            model_context_window: None,
            models: None,
        };
        let ledger = UsageLedgerRow::capture("agent", Some("thread"), observation);
        let mut row = serde_json::to_value(&ledger).expect("counter ledger");
        row["agentId"] = json!("agent");
        row["threadId"] = json!("thread");
        assert_eq!(
            UsageLedgerRow::from_typed_analytics(&row).expect("matching owner"),
            ledger
        );
        row["threadId"] = json!("other thread");
        assert!(UsageLedgerRow::from_typed_analytics(&row).is_err());
        row["threadId"] = Value::Null;
        assert!(UsageLedgerRow::from_typed_analytics(&row).is_err());
        row.as_object_mut().expect("object").remove("threadId");
        assert!(UsageLedgerRow::from_typed_analytics(&row).is_err());
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
pub fn legacy_era(ledger_since: LedgerEpoch, at: f64, estimated: bool) -> bool {
    match ledger_since {
        LedgerEpoch::Legacy => true,
        LedgerEpoch::Current => false,
        LedgerEpoch::Since(since) => estimated || at < since,
    }
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
