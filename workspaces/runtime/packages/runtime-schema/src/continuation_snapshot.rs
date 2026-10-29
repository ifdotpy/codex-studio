//! Typed persisted continuation snapshot. The snapshot schema owns the wire shape.
use crate::queue_command::Field;
use crate::{
    json_object::JsonObject, provider_kind::ProviderKind, recovery_record::SafeInteger,
    shared_values::MAX_SAFE_INTEGER, stored_record::StoredJsonInteger,
};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

/// A JSON string limited in Unicode code points.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedString<const MIN: usize, const MAX: usize>(String);
impl<const MIN: usize, const MAX: usize> BoundedString<MIN, MAX> {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de, const MIN: usize, const MAX: usize> Deserialize<'de> for BoundedString<MIN, MAX> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let n = s.chars().count();
        if (MIN..=MAX).contains(&n) {
            Ok(Self(s))
        } else {
            Err(D::Error::custom("string length out of range"))
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedVec<T, const MAX: usize>(Vec<T>);
impl<T, const MAX: usize> BoundedVec<T, MAX> {
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}
impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Vec::<T>::deserialize(d)?;
        if v.len() <= MAX {
            Ok(Self(v))
        } else {
            Err(D::Error::custom("too many items"))
        }
    }
}
/// A finite JSON number without schema bounds.
pub struct FiniteNumber(f64);
impl FiniteNumber {
    pub fn get(&self) -> f64 {
        self.0
    }
}
impl<'de> Deserialize<'de> for FiniteNumber {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(d)?;
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err(D::Error::custom("expected finite number"))
        }
    }
}
/// Nonnegative time, bounded to the shared safe integer maximum.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedNumber(Number);
impl BoundedNumber {
    pub fn get(&self) -> f64 {
        self.0.as_f64().expect("bounded JSON number")
    }
}
impl<'de> Deserialize<'de> for BoundedNumber {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let number = Number::deserialize(d)?;
        if number
            .as_f64()
            .is_some_and(|n| (0.0..=MAX_SAFE_INTEGER as f64).contains(&n))
        {
            Ok(Self(number))
        } else {
            Err(D::Error::custom("number out of range"))
        }
    }
}
/// Creation timestamp must be positive.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PositiveNumber(Number);
impl PositiveNumber {
    pub fn get(&self) -> f64 {
        self.0.as_f64().expect("positive JSON number")
    }
}
impl<'de> Deserialize<'de> for PositiveNumber {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = BoundedNumber::deserialize(d)?;
        if n.get() > 0.0 {
            Ok(Self(n.0))
        } else {
            Err(D::Error::custom("number must be positive"))
        }
    }
}
fn optional_nonnull<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn optional_string_enum<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    crate::string_enum::deserialize(d).map(Some)
}
fn required_nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}

pub type ContinuationSnapshotRecord = JsonObject<ContinuationSnapshot>;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContinuationRecord {
    Usage(UsageContinuation),
    Capacity(CapacityContinuation),
    Auth(AuthWaitReceipt),
}
impl ContinuationRecord {
    pub fn extra_mut(&mut self) -> &mut BTreeMap<String, Value> {
        match self {
            Self::Usage(record) => &mut record.extra,
            Self::Capacity(record) => &mut record.extra,
            Self::Auth(record) => &mut record.extra,
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Usage(record) => record.id.as_str(),
            Self::Capacity(record) => record.id.as_str(),
            Self::Auth(record) => record.id.as_str(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuationSnapshot {
    pub agent: ContinuationAgent,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub ancestors: Option<BoundedVec<ContinuationAgent, 64>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub assets: Option<BTreeMap<String, BTreeMap<String, Value>>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub claims: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub complaints: Option<BTreeMap<String, WakeComplaint>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub events: Option<BoundedVec<ContinuationEvent, 4096>>,
    pub limits: Option<JsonObject<ContinuationLimitsObservation>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub metadata: Option<BTreeMap<String, Option<ContinuationMetadata>>>,
    pub record: Option<ContinuationRecord>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub work: Option<BTreeMap<String, WakeWork>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthWaitReceipt {
    pub account_key: BoundedString<1, 4096>,
    pub account_pin: BTreeMap<String, Value>,
    pub agent: BoundedString<1, 4096>,
    pub attempt: ContinuationAttempt,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub cause: AuthWaitReceiptCause,
    pub created_at: PositiveNumber,
    #[serde(deserialize_with = "required_nullable")]
    pub due_at: Option<BoundedNumber>,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub error: AuthWaitReceiptError,
    pub id: BoundedString<1, 4096>,
    pub inputs: BoundedVec<BTreeMap<String, Value>, 64>,
    pub source: BTreeMap<String, Value>,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: AuthWaitReceiptStatus,
    pub task_claims: BoundedVec<BoundedString<1, 4096>, 4096>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityContinuation {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub accepted_turn_id: Field<BoundedString<1, 4096>>,
    pub account_key: BoundedString<1, 4096>,
    pub attempt: SafeInteger,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub cause: CapacityContinuationCause,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub claimed_at: Field<BoundedNumber>,
    pub cwd: BoundedString<0, 1048576>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub due_at: Field<BoundedNumber>,
    pub epoch: SafeInteger,
    pub id: BoundedString<1, 4096>,
    pub max_attempts: SafeInteger,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub reason: Field<BoundedString<0, 1048576>>,
    pub settings: BTreeMap<String, Value>,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: CapacityContinuationStatus,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    pub task_claims: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    pub thread_id: BoundedString<1, 4096>,
    pub turn_id: BoundedString<1, 4096>,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    pub waits: Option<SafeInteger>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComplaintResponse {
    pub author: BoundedString<1, 4096>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationAgent {
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub account_key: Option<BoundedString<1, 4096>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub auth_resume_attempt: Option<SafeInteger>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub auto_wake: Option<bool>,
    pub capacity_retry: Option<CapacityContinuation>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub capacity_retry_count: Option<SafeInteger>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub cwd: Option<BoundedString<0, 1048576>>,
    pub deleted_at: Option<BoundedNumber>,
    pub epoch: SafeInteger,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub excluded_account_keys: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    pub id: BoundedString<1, 4096>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub in_flight: Option<bool>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub is_lead: Option<bool>,
    pub last_completed_turn: Option<BoundedString<1, 4096>>,
    pub last_completed_turn_status: Option<String>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub native_failure_hold: Option<bool>,
    pub parent_id: Option<BoundedString<1, 4096>>,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub provider: ProviderKind,
    pub root_id: BoundedString<1, 4096>,
    pub start_attempt: Option<ContinuationAttempt>,
    pub status: BoundedString<1, 4096>,
    pub thread_id: Option<BoundedString<1, 4096>>,
    pub turn_epoch: Option<SafeInteger>,
    pub turn_id: Option<BoundedString<1, 4096>>,
    pub usage_resume: Option<UsageContinuation>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub usage_resume_enabled: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationAttempt {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub action: Field<String>,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_at_reservation: Option<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub capacity_retry_id: Field<BoundedString<1, 4096>>,
    pub epoch: SafeInteger,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    pub events: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    pub id: BoundedString<1, 4096>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub observed_turn_id: Field<BoundedString<1, 4096>>,
    pub submitted: bool,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub turn_id: Field<BoundedString<1, 4096>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationEvent {
    pub agent: BoundedString<1, 4096>,
    pub created: BoundedNumber,
    pub epoch: SafeInteger,
    pub error: Option<BoundedString<0, 1048576>>,
    pub id: BoundedString<1, 4096>,
    pub kind: BoundedString<1, 4096>,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: ContinuationEventStatus,
    pub text: BoundedString<0, 1048576>,
    #[serde(rename = "turn_id")]
    pub turn_id: Option<BoundedString<1, 4096>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuationLimitsObservation {
    #[serde(rename = "_syncEntities")]
    pub sync_entities: Option<Vec<JsonObject<SyncEntity>>>,
    #[serde(rename = "_syncEntitiesAfter")]
    sync_entities_after: Option<StoredJsonInteger>,
    pub account_key: String,
    pub at: Option<FiniteNumber>,
    pub checked_at: Option<FiniteNumber>,
    pub data: Option<UsageLimitsData>,
    pub error: Option<String>,
    pub processed_at: Option<FiniteNumber>,
    pub read_at: Option<FiniteNumber>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub stale: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationMetadata {
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub assets: Option<BoundedVec<BoundedString<1, 4096>, 64>>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub earlier_progress_updates: Option<SafeInteger>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub preserve_progress: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitBucket {
    pub individual_limit: Option<JsonObject<RateLimitIndividualLimit>>,
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub primary: Option<JsonObject<RateLimitWindow>>,
    pub rate_limit_reached_type: Option<String>,
    pub secondary: Option<JsonObject<RateLimitWindow>>,
    pub spend_control_reached: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateLimitIndividualLimit {
    pub remaining_percent: Option<FiniteNumber>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitResetCredit {
    pub expires_at: Option<FiniteNumber>,
    pub granted_at: Option<FiniteNumber>,
    pub id: String,
    pub reset_type: String,
    pub status: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitResetCredits {
    available_count: Option<StoredJsonInteger>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub credits: Option<Vec<RateLimitResetCredit>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateLimitWindow {
    pub resets_at: Option<FiniteNumber>,
    pub used_percent: Option<FiniteNumber>,
    pub window_duration_mins: Option<FiniteNumber>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyncEntity {
    #[serde(rename = "_deleted")]
    pub deleted: bool,
    pub id: String,
    pub payload: String,
    seq: StoredJsonInteger,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageContinuation {
    pub account_key: BoundedString<1, 4096>,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    pub auth_attempt: Option<SafeInteger>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub auth_refresh_marker: Field<BoundedString<0, 1048576>>,
    #[serde(
        default,
        deserialize_with = "optional_string_enum",
        skip_serializing_if = "Option::is_none"
    )]
    pub cause: Option<UsageContinuationCause>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub due_at: Field<BoundedNumber>,
    pub epoch: SafeInteger,
    pub failed_at: BoundedNumber,
    pub id: BoundedString<1, 4096>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub planned_at: Field<BoundedNumber>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub proof_at: Field<BoundedNumber>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub reason: Field<BoundedString<0, 1048576>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub reset_at: Field<BoundedNumber>,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: UsageContinuationStatus,
    #[serde(
        default,
        deserialize_with = "optional_nonnull",
        skip_serializing_if = "Option::is_none"
    )]
    pub task_claims: Option<BoundedVec<BoundedString<1, 4096>, 4096>>,
    pub thread_id: BoundedString<1, 4096>,
    pub turn_id: BoundedString<1, 4096>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitsData {
    pub account_id: Option<String>,
    pub ordinary_usage_allowed: Option<bool>,
    pub rate_limit_reset_credits: Option<RateLimitResetCredits>,
    pub rate_limits: Option<RateLimitBucket>,
    pub rate_limits_by_limit_id: Option<BTreeMap<String, RateLimitBucket>>,
    pub signed_in: Option<bool>,
    pub source: Option<String>,
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeComplaint {
    pub id: BoundedString<1, 4096>,
    pub lead_id: BoundedString<1, 4096>,
    pub recipient: String,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub responses: Option<BoundedVec<ComplaintResponse, 4096>>,
    pub source_type: Option<String>,
    pub status: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeWork {
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub decisions: Option<BoundedVec<WorkDecision, 4096>>,
    pub id: BoundedString<1, 4096>,
    #[serde(default, deserialize_with = "optional_nonnull")]
    pub results: Option<BoundedVec<WorkResult, 4096>>,
    pub root_id: BoundedString<1, 4096>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkDecision {
    pub result_id: BoundedString<1, 4096>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkResult {
    pub id: BoundedString<1, 4096>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthWaitReceiptCause {
    #[serde(rename = "claude_auth_probe")]
    ClaudeAuthProbe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthWaitReceiptError {
    #[serde(rename = "Cannot read Claude Code sign-in status")]
    CannotReadClaudeCodeSignInStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthWaitReceiptStatus {
    #[serde(rename = "auth_probe_wait")]
    AuthProbeWait,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "cancelled")]
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapacityContinuationCause {
    #[serde(rename = "serverOverloaded")]
    ServerOverloaded,
    #[serde(rename = "internalServerError")]
    InternalServerError,
    #[serde(rename = "httpConnectionFailed")]
    HttpConnectionFailed,
    #[serde(rename = "responseStreamConnectionFailed")]
    ResponseStreamConnectionFailed,
    #[serde(rename = "responseStreamDisconnected")]
    ResponseStreamDisconnected,
    #[serde(rename = "responseTooManyFailedAttempts")]
    ResponseTooManyFailedAttempts,
    #[serde(rename = "diskFull")]
    DiskFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapacityContinuationStatus {
    #[serde(rename = "scheduled")]
    Scheduled,
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "finished")]
    Finished,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "exhausted")]
    Exhausted,
    #[serde(rename = "failed")]
    Failed,
}

#[derive(Deserialize)]
pub enum ContinuationEventStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "reserved")]
    Reserved,
    #[serde(rename = "dispatching")]
    Dispatching,
    #[serde(rename = "uncertain")]
    Uncertain,
    #[serde(rename = "delivered")]
    Delivered,
    #[serde(rename = "stored_only")]
    StoredOnly,
    #[serde(rename = "cancelled")]
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub enum UsageContinuationCause {
    #[serde(rename = "usage_limit")]
    UsageLimit,
    #[serde(rename = "rate_limit")]
    RateLimit,
    #[serde(rename = "auth")]
    Auth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub enum UsageContinuationStatus {
    #[serde(rename = "scheduled")]
    Scheduled,
    #[serde(rename = "started")]
    Started,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "unknown")]
    Unknown,
}

impl ContinuationLimitsObservation {
    pub fn sync_entities_after(&self) -> Option<&serde_json::Number> {
        self.sync_entities_after
            .as_ref()
            .map(StoredJsonInteger::number)
    }
}
impl RateLimitResetCredits {
    pub fn available_count(&self) -> Option<&serde_json::Number> {
        self.available_count.as_ref().map(StoredJsonInteger::number)
    }
}
impl SyncEntity {
    pub fn seq(&self) -> &serde_json::Number {
        self.seq.number()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    #[test]
    fn continuation_record_owners_preserve_numbers_presence_and_extensions() {
        let usage = json!({"id":"u","accountKey":"a","threadId":"t","epoch":1.0,"turnId":"v","status":"scheduled","cause":"auth","failedAt":100,"dueAt":null,"proofAt":3.0,"extension":{"a":[1,null]}});
        let capacity = json!({"id":"c","accountKey":"a","threadId":"t","epoch":1.0,"turnId":"v","status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{},"attempt":1.0,"maxAttempts":3,"claimedAt":null,"extension":{"a":[1,null]}});
        let u = UsageContinuation::deserialize(usage.clone()).unwrap();
        let c = CapacityContinuation::deserialize(capacity.clone()).unwrap();
        assert_eq!(json!(u), usage);
        assert_eq!(json!(c), capacity);
        let auth = json!({"id":"a","agent":"a","accountKey":"x","status":"auth_probe_wait","cause":"claude_auth_probe","error":"Cannot read Claude Code sign-in status","createdAt":1,"dueAt":null,"source":{},"attempt":{"id":"x","epoch":1.0,"submitted":false,"action":null},"inputs":[],"accountPin":{},"taskClaims":[],"future":true});
        let a = AuthWaitReceipt::deserialize(auth.clone()).unwrap();
        assert_eq!(json!(a), auth);
    }

    #[test]
    fn continuation_schema_parity() {
        let text = include_str!("schemas/persisted-continuation-snapshot.schema.json");
        assert_eq!(
            data_encoding::HEXLOWER.encode(&Sha256::digest(text.as_bytes())),
            "ea22a78712088363b0d45b94d80f3d8b62bc350c3f026842f7e27a2fda33b842",
            "schema changed: review continuation_snapshot typed owner"
        );
        let schema: Value = serde_json::from_str(text).expect("schema");
        let samples = [
            json!(null),
            json!(true),
            json!(false),
            json!(""),
            json!("x"),
            json!(-1),
            json!(0),
            json!(1),
            json!(1.0),
            json!(0.5),
            json!(9007199254740991_u64),
            json!(9007199254740992_u64),
            json!([]),
            json!(["x"]),
            json!([1]),
            json!({}),
            json!({"extra":true}),
        ];
        type Parse = fn(Value) -> bool;
        let cases: Vec<(&str, Value, Parse)> = vec![
            (
                "ROOT",
                json!({"agent":{"id":"x","rootId":"x","epoch":0,"status":"x","provider":"codex"}}),
                |v| serde_json::from_value::<ContinuationSnapshotRecord>(v).is_ok(),
            ),
            (
                "AuthWaitReceipt",
                json!({"id":"x","agent":"x","accountKey":"x","status":"auth_probe_wait","cause":"claude_auth_probe","error":"Cannot read Claude Code sign-in status","createdAt":1,"dueAt":0,"source":{},"attempt":{"id":"x","epoch":0,"submitted":false},"inputs":[],"accountPin":{},"taskClaims":[]}),
                |v| serde_json::from_value::<AuthWaitReceipt>(v).is_ok(),
            ),
            (
                "CapacityContinuation",
                json!({"id":"x","accountKey":"x","threadId":"x","epoch":0,"turnId":"x","status":"scheduled","cause":"serverOverloaded","cwd":"","settings":{},"attempt":0,"maxAttempts":0}),
                |v| serde_json::from_value::<CapacityContinuation>(v).is_ok(),
            ),
            ("ComplaintResponse", json!({"author":"x"}), |v| {
                serde_json::from_value::<ComplaintResponse>(v).is_ok()
            }),
            (
                "ContinuationAgent",
                json!({"id":"x","rootId":"x","epoch":0,"status":"x","provider":"codex"}),
                |v| serde_json::from_value::<ContinuationAgent>(v).is_ok(),
            ),
            (
                "ContinuationAttempt",
                json!({"id":"x","epoch":0,"submitted":false}),
                |v| serde_json::from_value::<ContinuationAttempt>(v).is_ok(),
            ),
            (
                "ContinuationEvent",
                json!({"id":"x","agent":"x","epoch":0,"kind":"x","text":"","status":"pending","created":0}),
                |v| serde_json::from_value::<ContinuationEvent>(v).is_ok(),
            ),
            (
                "ContinuationLimitsObservation",
                json!({"accountKey":""}),
                |v| serde_json::from_value::<JsonObject<ContinuationLimitsObservation>>(v).is_ok(),
            ),
            ("ContinuationMetadata", json!({}), |v| {
                serde_json::from_value::<ContinuationMetadata>(v).is_ok()
            }),
            ("RateLimitBucket", json!({}), |v| {
                serde_json::from_value::<RateLimitBucket>(v).is_ok()
            }),
            ("RateLimitIndividualLimit", json!({}), |v| {
                serde_json::from_value::<JsonObject<RateLimitIndividualLimit>>(v).is_ok()
            }),
            (
                "RateLimitResetCredit",
                json!({"id":"","status":"","resetType":""}),
                |v| serde_json::from_value::<RateLimitResetCredit>(v).is_ok(),
            ),
            ("RateLimitResetCredits", json!({}), |v| {
                serde_json::from_value::<RateLimitResetCredits>(v).is_ok()
            }),
            ("RateLimitWindow", json!({}), |v| {
                serde_json::from_value::<JsonObject<RateLimitWindow>>(v).is_ok()
            }),
            (
                "SyncEntity",
                json!({"id":"","seq":0,"payload":"","_deleted":false}),
                |v| serde_json::from_value::<JsonObject<SyncEntity>>(v).is_ok(),
            ),
            (
                "UsageContinuation",
                json!({"id":"x","accountKey":"x","threadId":"x","epoch":0,"turnId":"x","status":"scheduled","failedAt":0}),
                |v| serde_json::from_value::<UsageContinuation>(v).is_ok(),
            ),
            ("UsageLimitsData", json!({}), |v| {
                serde_json::from_value::<UsageLimitsData>(v).is_ok()
            }),
            (
                "WakeComplaint",
                json!({"id":"x","leadId":"x","recipient":"","status":""}),
                |v| serde_json::from_value::<WakeComplaint>(v).is_ok(),
            ),
            ("WakeWork", json!({"id":"x","rootId":"x"}), |v| {
                serde_json::from_value::<WakeWork>(v).is_ok()
            }),
            ("WorkDecision", json!({"resultId":"x"}), |v| {
                serde_json::from_value::<WorkDecision>(v).is_ok()
            }),
            ("WorkResult", json!({"id":"x"}), |v| {
                serde_json::from_value::<WorkResult>(v).is_ok()
            }),
        ];
        let mut enum_objects_accepted = Vec::new();
        for (name, base, parse) in cases {
            let target = if name == "ROOT" {
                schema.clone()
            } else {
                json!({"$defs":schema["$defs"], "$ref":format!("#/$defs/{name}")})
            };
            let validator = jsonschema::validator_for(&target).expect("validator");
            assert!(
                validator.is_valid(&base),
                "invalid test baseline {name}: {base}"
            );
            assert!(
                parse(base.clone()),
                "typed baseline rejected {name}: {base}"
            );
            let props = if name == "ROOT" {
                &schema["properties"]
            } else {
                &schema["$defs"][name]["properties"]
            };
            for key in props.as_object().expect("properties").keys() {
                let mut absent = base.clone();
                absent.as_object_mut().expect("object").remove(key);
                assert_eq!(
                    parse(absent.clone()),
                    validator.is_valid(&absent),
                    "{name} missing {key}: {absent}"
                );
                for sample in &samples {
                    let mut input = base.clone();
                    input[key] = sample.clone();
                    assert_eq!(
                        parse(input.clone()),
                        validator.is_valid(&input),
                        "{name} {key}: {input}"
                    );
                }
                let variants = props[key]["enum"].as_array().or_else(|| {
                    (props[key]["$ref"] == "#/$defs/ProviderKind")
                        .then(|| schema["$defs"]["ProviderKind"]["enum"].as_array())
                        .flatten()
                });
                if let Some(variants) = variants {
                    for variant in variants {
                        let mut input = base.clone();
                        input[key] = variant.clone();
                        assert!(validator.is_valid(&input));
                        assert!(parse(input.clone()), "{name} {key}: {input}");
                        let spelling = variant.as_str().expect("string enum spelling");
                        input[key] = json!({spelling:null});
                        assert!(!validator.is_valid(&input));
                        if parse(input.clone()) {
                            enum_objects_accepted.push(format!("{name}.{key}: {input}"));
                        }
                    }
                }
                if let Some(spelling) = props[key]["const"].as_str() {
                    let mut input = base.clone();
                    input[key] = json!({spelling:null});
                    assert!(!validator.is_valid(&input));
                    if parse(input.clone()) {
                        enum_objects_accepted.push(format!("{name}.{key}: {input}"));
                    }
                }
            }
            for input in [json!([]), json!({"unknown":[1,null]}), json!(null)] {
                assert_eq!(
                    parse(input.clone()),
                    validator.is_valid(&input),
                    "{name}: {input}"
                );
            }
        }
        assert!(
            enum_objects_accepted.is_empty(),
            "accepted enum objects: {enum_objects_accepted:?}"
        );
    }
}
