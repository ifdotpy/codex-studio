use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::StoreResult;

#[cfg(test)]
pub(crate) mod validation_counts {
    use std::cell::Cell;
    thread_local! {
        pub static CHECKS: Cell<usize> = const { Cell::new(0) };
        pub static ENCODINGS: Cell<usize> = const { Cell::new(0) };
    }
}

macro_rules! row_key_accessor {
    ($name:ident, $id:ty, PayloadId) => {
        impl $name {
            pub fn id(&self) -> &$id {
                // `from_value` caches the required payload ID before a record
                // can enter a typed table operation.
                self.row_key.as_ref().expect("payload-id record has no id")
            }
        }
    };
    ($name:ident, $id:ty, External) => {};
}

macro_rules! record_newtype {
    ($name:ident, $id:ty, table = $table:literal, schema = $schema:literal,
     key = $key:ident, query = $query:ident) => {
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name {
            row_key: Option<$id>,
            value: Value,
        }

        impl $name {
            pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
                let row_key = match RecordKeyMode::$key {
                    RecordKeyMode::PayloadId => Some(<$id>::from(record_id(&value)?)),
                    RecordKeyMode::External => None,
                };
                Ok(Self { row_key, value })
            }

            pub fn value(&self) -> &Value {
                &self.value
            }

            pub fn into_value(self) -> Value {
                self.value
            }

            pub fn get(&self, key: &str) -> Option<&Value> {
                self.value.get(key)
            }

            pub fn set(&mut self, key: impl Into<String>, value: Value) {
                if let Some(object) = self.value.as_object_mut() {
                    object.insert(key.into(), value);
                }
            }

            pub fn remove(&mut self, key: &str) -> Option<Value> {
                self.value.as_object_mut()?.remove(key)
            }
        }

        row_key_accessor!($name, $id, $key);

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                self.value.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Self::from_value(Value::deserialize(deserializer)?)
                    .map_err(serde::de::Error::custom)
            }
        }

        impl JsonRecord for $name {
            type Id = $id;

            const TABLE: &'static str = $table;
            const SCHEMA: &'static str = $schema;
            const KEY_MODE: RecordKeyMode = RecordKeyMode::$key;
            const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::$query;

            fn record_key(&self) -> Option<&Self::Id> {
                self.row_key.as_ref()
            }
        }
    };
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(AgentId);
string_id!(TaskId);
string_id!(QueueEventId);
string_id!(ChatMessageId);
string_id!(TranscriptItemId);
string_id!(TranscriptAssetId);

/// A future agent status that is distinct from every known status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownAgentStatus(String);

impl UnknownAgentStatus {
    pub fn new(value: String) -> Option<Self> {
        (!AgentStatus::VALUES.contains(&value.as_str())).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! stored_string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub enum $name {
            $($variant,)+
            Other(UnknownAgentStatus),
        }

        impl $name {
            /// Wire values owned by this enum; generated contracts and parity witnesses consume these.
            pub const VALUES: &'static [&'static str] = &[$($value,)+];

            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $value,)+
                    Self::Other(value) => value.as_str(),
                }
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                match self {
                    $(Self::$variant => serializer.serialize_str($value),)+
                    Self::Other(value) => serializer.serialize_str(value.as_str()),
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Ok(Self::parse(&value))
            }
        }

        impl $name {
            fn parse(value: &str) -> Self {
                match value {
                    $($value => Self::$variant,)+
                    _ => Self::Other(UnknownAgentStatus(value.to_owned())),
                }
            }
        }
    };
}

stored_string_enum!(AgentStatus {
    Unknown => "unknown",
    Idle => "idle",
    Queued => "queued",
    Starting => "starting",
    Running => "running",
    Waiting => "waiting",
    WaitingOnMonitor => "waiting_on_monitor",
    Approval => "approval",
    Completed => "completed",
    Interrupted => "interrupted",
    Failed => "failed",
    Blocked => "blocked",
    Paused => "paused",
    Parked => "parked",
    Abandoned => "abandoned",
    CapacityRetry => "capacity-retry",
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub id: AgentId,
    pub root_id: AgentId,
    pub epoch: i64,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_job: Option<StoredTitleJob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_outcome_hold: Option<StoredStartOutcomeHold>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub fired_events: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_review_thread: Option<NativeReviewThread>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_launch: Option<VmLaunch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_copy: Option<CredentialCopy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_auth_bootstrap: Option<VmAuthBootstrap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_release: Option<NativeRelease>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeReviewThread {
    pub sequence: u64,
    pub attempt: String,
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_request_id: Option<String>,
    #[serde(default)]
    pub admitted_request: Option<crate::request_results::AdmittedRequest>,
    pub generation: u64,
    pub connection: String,
    pub request_digest: String,
    pub status: studio_runtime_schema::persistence::NativeReviewThreadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIdentity {
    pub agent_id: String,
    pub epoch: i64,
    pub handle: String,
    pub request_id: String,
    pub launch_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmLaunch {
    pub identity: StartIdentity,
    pub signature: String,
    pub status: studio_runtime_schema::persistence::WorkEffectStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialCopy {
    pub agent_id: String,
    pub epoch: i64,
    pub account_key: String,
    pub provider: studio_runtime_providers::ProviderKind,
    pub relative_path: String,
    pub digest: String,
    pub request_id: String,
    pub signature: String,
    pub status: studio_runtime_schema::persistence::WorkEffectStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmAuthBootstrap {
    pub agent_id: String,
    pub epoch: i64,
    pub request_id: String,
    pub root_request_id: String,
    pub admitted_request: crate::request_results::AdmittedRequest,
    pub fingerprint: String,
    pub state: studio_runtime_schema::persistence::WorkEffectStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeReleaseScope {
    pub account: String,
    #[serde(flatten)]
    pub provider: studio_runtime_schema::persistence::ProviderScope,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeRelease {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_request: Option<crate::request_results::AdmittedRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<NativeReleaseScope>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleJob {
    pub request_id: String,
    pub agent_id: String,
    pub root_request_id: String,
    pub admitted_request: crate::request_results::AdmittedRequest,
    pub status: studio_runtime_schema::persistence::TitleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredTitleJob {
    Job(Box<TitleJob>),
    Unrecognized(Value),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum StartOutcomeHold {
    Held {
        #[serde(rename = "operationId", skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        #[serde(
            rename = "connectionGeneration",
            skip_serializing_if = "Option::is_none"
        )]
        connection_generation: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(rename = "attemptId", skip_serializing_if = "Option::is_none")]
        attempt_id: Option<String>,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    Recovery {
        epoch: i64,
        reason: String,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    AccountSettlement {
        cursor: Option<u64>,
        reason: String,
        #[serde(rename = "previousError")]
        previous_error: Value,
        #[serde(rename = "previousNativeFailureHold")]
        previous_native_failure_hold: PreviousNativeFailureHold,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviousNativeFailureHold {
    pub present: bool,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredStartOutcomeHold {
    Known(StartOutcomeHold),
    Unrecognized(Value),
}

impl StoredStartOutcomeHold {
    pub fn operation_id(&self) -> Option<&str> {
        match self {
            Self::Known(StartOutcomeHold::Held { operation_id, .. }) => operation_id.as_deref(),
            Self::Known(
                StartOutcomeHold::Recovery { .. } | StartOutcomeHold::AccountSettlement { .. },
            )
            | Self::Unrecognized(_) => None,
        }
    }

    pub fn attempt_id(&self) -> Option<&str> {
        match self {
            Self::Known(StartOutcomeHold::Held { attempt_id, .. }) => attempt_id.as_deref(),
            Self::Known(
                StartOutcomeHold::Recovery { .. } | StartOutcomeHold::AccountSettlement { .. },
            )
            | Self::Unrecognized(_) => None,
        }
    }

    pub fn is_account_settlement(&self) -> bool {
        matches!(
            self,
            Self::Known(StartOutcomeHold::AccountSettlement { .. })
        )
    }
}

/// Typed projection of the scheduler-owned fields in a start attempt.
/// Other owners retain their fields in the original object.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartAttemptView {
    pub id: Option<String>,
    pub epoch: Option<i64>,
    pub account_key: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub observed_turn_id: Option<String>,
    pub events: Option<Vec<String>>,
    pub submitted: Option<bool>,
    pub active_at_reservation: Option<bool>,
    pub created: Option<f64>,
    #[serde(default, deserialize_with = "start_attempt_action")]
    pub action: Option<StartAttemptAction>,
    pub execution_outcome: Option<studio_runtime_schema::persistence::ExecutionStatus>,
    pub request_ticket: Option<String>,
    pub connection_id: Option<String>,
    pub connection_generation: Option<u64>,
    pub native_operation_id: Option<String>,
    pub root_request_id: Option<String>,
    pub interrupt: Option<StartInterruptView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartAttemptAction {
    Compact,
    Review,
    Capacity,
    Safety,
}

fn start_attempt_action<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<StartAttemptAction>, D::Error> {
    StartAttemptAction::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptOutcome {
    Unknown,
    Unsent,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartInterruptView {
    pub request_ticket: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub generation: Option<u64>,
    pub outcome: Option<InterruptOutcome>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveExecutionView {
    pub run_id: Option<String>,
    pub attempt_id: Option<String>,
}

macro_rules! start_attempt_setters {
    ($($name:ident: $ty:ty = $field:literal;)+) => {
        $(pub fn $name(&mut self, value: $ty) -> StoreResult<()> {
            self.set_start_attempt_field($field, &value)
        })+
    };
}

impl AgentRecord {
    pub fn active_execution(&self) -> Result<Option<ActiveExecutionView>, serde_json::Error> {
        self.extra
            .get("activeExecution")
            .filter(|value| !value.is_null())
            .map(ActiveExecutionView::deserialize)
            .transpose()
    }

    fn set_active_execution_field<T: Serialize>(
        &mut self,
        field: &str,
        value: &T,
    ) -> StoreResult<()> {
        let pointer = self
            .extra
            .entry("activeExecution".into())
            .or_insert_with(|| serde_json::Map::new().into());
        if pointer.is_null() {
            *pointer = serde_json::Map::new().into();
        }
        let pointer = pointer
            .as_object_mut()
            .ok_or(crate::StoreError::RecordContract(
                "invalid active execution",
            ))?;
        pointer.insert(field.into(), crate::projection_value(value));
        Ok(())
    }
    pub fn set_active_execution_run_id(&mut self, value: &str) -> StoreResult<()> {
        self.set_active_execution_field("runId", &value)
    }
    pub fn set_active_execution_attempt_id(&mut self, value: Option<&str>) -> StoreResult<()> {
        self.set_active_execution_field("attemptId", &value)
    }

    pub fn start_attempt(&self) -> Result<Option<StartAttemptView>, serde_json::Error> {
        self.extra
            .get("startAttempt")
            .filter(|value| !value.is_null())
            .map(StartAttemptView::deserialize)
            .transpose()
    }

    fn set_start_attempt_field<T: Serialize>(&mut self, field: &str, value: &T) -> StoreResult<()> {
        let attempt = self
            .extra
            .get_mut("startAttempt")
            .and_then(|value| value.as_object_mut())
            .ok_or(crate::StoreError::RecordContract("missing start attempt"))?;
        attempt.insert(field.into(), crate::projection_value(value));
        Ok(())
    }

    start_attempt_setters! {
        set_start_attempt_submitted: bool = "submitted";
        set_start_attempt_account_key: &str = "accountKey";
        set_start_attempt_thread_id: Option<&str> = "threadId";
        set_start_attempt_connection_id: Option<&str> = "connectionId";
        set_start_attempt_native_operation_id: &str = "nativeOperationId";
        set_start_attempt_not_submitted_reason: &str = "notSubmittedReason";
        set_start_attempt_retired_events: &[String] = "retiredEvents";
        set_start_attempt_turn_id: Option<&str> = "turnId";
        set_start_attempt_observed_turn_id: Option<&str> = "observedTurnId";
        set_start_attempt_execution_outcome: studio_runtime_schema::persistence::ExecutionStatus = "executionOutcome";
        set_start_attempt_request_ticket: &str = "requestTicket";
        set_start_attempt_connection_generation: u64 = "connectionGeneration";
    }

    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    pub fn id(&self) -> &AgentId {
        &self.id
    }

    pub fn value(&self) -> Value {
        let mut value = Value::Object(self.extra.clone().into_iter().collect());
        value["id"] = Value::String(self.id.0.clone());
        value["rootId"] = Value::String(self.root_id.0.clone());
        value["epoch"] = Value::from(self.epoch);
        value["status"] = Value::String(self.status.as_str().to_owned());
        if let Some(job) = &self.title_job {
            value["titleJob"] = crate::projection_value(job);
        }
        if let Some(hold) = &self.start_outcome_hold {
            value["startOutcomeHold"] = crate::projection_value(hold);
        }
        if !self.fired_events.is_empty() {
            value["firedEvents"] = crate::projection_value(&self.fired_events);
        }
        if let Some(field) = &self.native_review_thread {
            value["nativeReviewThread"] = crate::projection_value(field);
        }
        if let Some(field) = &self.vm_launch {
            value["vmLaunch"] = crate::projection_value(field);
        }
        if let Some(field) = &self.credential_copy {
            value["credentialCopy"] = crate::projection_value(field);
        }
        if let Some(field) = &self.vm_auth_bootstrap {
            value["vmAuthBootstrap"] = crate::projection_value(field);
        }
        if let Some(field) = &self.native_release {
            value["nativeRelease"] = crate::projection_value(field);
        }
        value
    }

    /// Consume the typed record while moving its fields into a JSON object.
    pub fn into_value(self) -> Value {
        let mut value = Value::Object(self.extra.into_iter().collect());
        value["id"] = Value::String(self.id.0);
        value["rootId"] = Value::String(self.root_id.0);
        value["epoch"] = Value::from(self.epoch);
        value["status"] = Value::String(self.status.as_str().to_owned());
        if let Some(job) = &self.title_job {
            value["titleJob"] = crate::projection_value(job);
        }
        if let Some(hold) = &self.start_outcome_hold {
            value["startOutcomeHold"] = crate::projection_value(hold);
        }
        if !self.fired_events.is_empty() {
            value["firedEvents"] = crate::projection_value(&self.fired_events);
        }
        if let Some(field) = &self.native_review_thread {
            value["nativeReviewThread"] = crate::projection_value(field);
        }
        if let Some(field) = &self.vm_launch {
            value["vmLaunch"] = crate::projection_value(field);
        }
        if let Some(field) = &self.credential_copy {
            value["credentialCopy"] = crate::projection_value(field);
        }
        if let Some(field) = &self.vm_auth_bootstrap {
            value["vmAuthBootstrap"] = crate::projection_value(field);
        }
        if let Some(field) = &self.native_release {
            value["nativeRelease"] = crate::projection_value(field);
        }
        value
    }

    pub fn set_status(&mut self, status: AgentStatus) {
        self.status = status;
    }
}

impl JsonRecord for AgentRecord {
    type Id = AgentId;

    const TABLE: &'static str = "runtime_agents";
    const SCHEMA: &'static str = "PersistedAgentRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;

    fn record_key(&self) -> Option<&Self::Id> {
        Some(&self.id)
    }
}

pub use studio_runtime_schema::records::WorkStatus;

record_newtype!(
    TaskRecord,
    TaskId,
    table = "runtime_tasks",
    schema = "PersistedTaskRecord",
    key = PayloadId,
    query = JsonAgent
);
pub use studio_runtime_schema::plan_record::PlanRecord;
impl JsonRecord for PlanRecord {
    type Id = AgentId;
    const TABLE: &'static str = "runtime_plans";
    const SCHEMA: &'static str = "PersistedPlanRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::External;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    const DIRECT_TYPED_DECODE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        None
    }
}
#[cfg(test)]
mod plan_typed_tests {
    use super::{AgentId, PlanRecord, Table};
    use serde_json::json;

    #[test]
    fn plan_deserialization_enforces_shape_and_preserves_extensions() {
        for invalid in [
            json!({"id":"a"}),
            json!({"id":"a","rootId":"r","text":"","version":1.5,"updated":null}),
            json!({"id":"a","rootId":"r","text":"","version":1,"updated":null,"native":[]}),
        ] {
            assert!(serde_json::from_value::<PlanRecord>(invalid).is_err());
        }
        let value = json!({"id":"different-from-row","rootId":"r","text":"x","version":1.0,"updated":null,"native":null,"future":[1]});
        let record: PlanRecord = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(record).unwrap(), value);
    }

    #[test]
    fn plan_store_uses_external_key_and_rejects_duplicate_fields() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_plans(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        let value = json!({"id":"payload-id","rootId":"root","text":"x","version":1.0,"updated":null,"future":true});
        let record = PlanRecord::from_value(value.clone()).unwrap();
        let key = AgentId::from("row-id");
        Table::<PlanRecord>::new(&db).put_at(&key, &record).unwrap();
        assert_eq!(
            Table::<PlanRecord>::new(&db)
                .get(&key)
                .unwrap()
                .unwrap()
                .into_value(),
            value
        );
        db.execute(
            "UPDATE runtime_plans SET record=?1",
            [r#"{"id":"a","id":"b","rootId":"r","text":"","version":0,"updated":null}"#],
        )
        .unwrap();
        assert!(Table::<PlanRecord>::new(&db).get(&key).is_err());
    }
}
pub use studio_runtime_schema::annotation_record::AnnotationRecord;
impl JsonRecord for AnnotationRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_annotations";
    const SCHEMA: &'static str = "PersistedAnnotationRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    const DIRECT_TYPED_DECODE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
#[cfg(test)]
mod annotation_typed_tests {
    use super::{AnnotationRecord, Table};
    use serde_json::json;

    #[test]
    fn annotation_deserialization_enforces_shapes() {
        for value in [
            json!(["a"]),
            json!({"id":"a","line":1.5}),
            json!({"id":"a","text":1}),
        ] {
            assert!(serde_json::from_value::<AnnotationRecord>(value).is_err());
        }
        for value in [
            json!({"id":"a"}),
            json!({"id":"a","line":1.0,"created":null,"extra":[1]}),
        ] {
            let record: AnnotationRecord = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(record).unwrap(), value);
        }
    }

    #[test]
    fn annotation_line_validator_domain() {
        for value in [json!(-1), json!(u64::MAX), json!(1.0)] {
            assert!(
                studio_runtime_schema::validate(
                    "PersistedAnnotationRecord",
                    &json!({"id":"a","line":value})
                )
                .unwrap(),
                "{value}"
            );
        }
    }

    #[test]
    fn annotation_store_preserves_identity_extensions_and_rejects_duplicate_keys() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_annotations(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .unwrap();
        let value = json!({"id":"a","line":-1.0,"created":null,"future":{"x":1}});
        let record = AnnotationRecord::from_value(value.clone()).unwrap();
        Table::<AnnotationRecord>::new(&db).put(&record).unwrap();
        assert_eq!(
            Table::<AnnotationRecord>::new(&db)
                .get(&"a".to_owned())
                .unwrap()
                .unwrap()
                .into_value(),
            value
        );
        db.execute(
            "UPDATE runtime_annotations SET record=?1 WHERE id='a'",
            [r#"{"id":"other","id":"a"}"#],
        )
        .unwrap();
        assert!(
            Table::<AnnotationRecord>::new(&db)
                .get(&"a".to_owned())
                .is_err()
        );
        db.execute(
            "UPDATE runtime_annotations SET record=?1 WHERE id='a'",
            [r#"{"id":"other"}"#],
        )
        .unwrap();
        assert!(
            Table::<AnnotationRecord>::new(&db)
                .get(&"a".to_owned())
                .is_err()
        );
    }
}
record_newtype!(
    ToolResultRecord,
    String,
    table = "request_results",
    schema = "PersistedToolResult",
    key = External,
    query = None
);
record_newtype!(
    RequestRecord,
    String,
    table = "runtime_requests",
    schema = "PersistedRequestRecord",
    key = PayloadId,
    query = JsonAgent
);
pub use studio_runtime_schema::complaint_record::ComplaintRecord;
impl JsonRecord for ComplaintRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_complaints";
    const SCHEMA: &'static str = "PersistedComplaintRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    const DIRECT_TYPED_DECODE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
#[cfg(test)]
mod complaint_typed_tests {
    use super::{ComplaintRecord, Table};
    use serde_json::json;

    #[test]
    fn complaint_deserialization_enforces_shape_and_preserves_open_status() {
        for value in [
            json!(["c"]),
            json!({"id":"c","author":1}),
            json!({"id":"c","status":false}),
            json!({"id":"c","leadId":[]}),
        ] {
            assert!(serde_json::from_value::<ComplaintRecord>(value).is_err());
        }
        for value in [
            json!({"id":"c"}),
            json!({"id":"c","author":null,"leadId":"l","status":"future","text":null,"future":[1]}),
        ] {
            let decoded: ComplaintRecord = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        }
    }

    #[test]
    fn complaint_storage_preserves_extensions_and_enforces_identity() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        let raw = r#"{ "id":"c", "status":"future", "author":null, "future":{"a":1} }"#;
        crate::QuestionsWriteRepository::put_complaint(&tx, "c", raw).unwrap();
        let saved: String = tx
            .query_row(
                "SELECT record FROM runtime_complaints WHERE id='c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved, raw);
        assert_eq!(
            Table::<ComplaintRecord>::new(&tx)
                .get(&"c".to_owned())
                .unwrap()
                .unwrap()
                .into_value(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
        assert!(crate::QuestionsWriteRepository::put_complaint(&tx, "other", raw).is_err());
        tx.execute(
            "UPDATE runtime_complaints SET record=?1",
            [r#"{"id":"other","id":"c"}"#],
        )
        .unwrap();
        assert!(
            Table::<ComplaintRecord>::new(&tx)
                .get(&"c".to_owned())
                .is_err()
        );
    }
}
pub use studio_runtime_schema::checkpoint_record::CheckpointRecord;
impl JsonRecord for CheckpointRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_checkpoints";
    const SCHEMA: &'static str = "PersistedCheckpointRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::JsonAgent;
    const TYPED_SHAPE: bool = true;
    const DIRECT_TYPED_DECODE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
record_newtype!(
    MonitorRecord,
    String,
    table = "runtime_monitors",
    schema = "PersistedMonitorRecord",
    key = PayloadId,
    query = JsonAgent
);
pub use studio_runtime_schema::terminal_record::TerminalRecord;

impl JsonRecord for TerminalRecord {
    type Id = String;
    const TABLE: &'static str = "user_terminals";
    const SCHEMA: &'static str = "PersistedTerminalRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}

#[cfg(test)]
mod terminal_typed_tests {
    use super::{Table, TerminalRecord};
    use serde_json::json;

    #[test]
    fn terminal_deserialization_enforces_schema_and_preserves_fields() {
        for invalid in [
            json!(["t"]),
            json!({"id":"t","agent":7}),
            json!({"id":"t","status":false}),
            json!({"id":"t","exitCode":1.5}),
        ] {
            assert!(serde_json::from_value::<TerminalRecord>(invalid).is_err());
        }
        for valid in [
            json!({"id":"t"}),
            json!({"id":"t","agent":null,"status":null,"exitCode":null,"extra":{"a":1}}),
            json!({"id":"t","exitCode":1.0}),
            json!({"id":"t","exitCode":u64::MAX}),
        ] {
            let record: TerminalRecord = serde_json::from_value(valid.clone()).unwrap();
            assert_eq!(serde_json::to_value(record).unwrap(), valid);
        }
    }

    #[test]
    fn terminal_storage_checks_row_identity_and_preserves_sql_columns() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE user_terminals(id TEXT PRIMARY KEY,record TEXT NOT NULL,output TEXT NOT NULL,offset INTEGER NOT NULL,output_generation INTEGER NOT NULL)").unwrap();
        let payload = json!({"id":"t","status":null,"exitCode":1.0,"future":{"value":true}});
        let record = TerminalRecord::from_value(payload.clone()).unwrap();
        let tx = db.transaction().unwrap();
        crate::TerminalsRepository::put(&tx, "t", &record, "hello", 4).unwrap();
        let row = crate::TerminalsRepository::get(&tx, "t").unwrap().unwrap();
        assert_eq!(row.record.value(), payload);
        assert_eq!(
            (row.output.as_str(), row.offset, row.output_generation),
            ("hello", 4, 0)
        );
        let mut conflicting = record.clone();
        assert!(conflicting.set("status", json!(false)).is_err());
        assert_eq!(conflicting.value(), payload);
        tx.execute(
            "UPDATE user_terminals SET record=?1 WHERE id='t'",
            [json!({"id":"other"}).to_string()],
        )
        .unwrap();
        assert!(
            Table::<TerminalRecord>::new(&tx)
                .get(&"t".to_string())
                .is_err()
        );
        tx.execute(
            "UPDATE user_terminals SET record=?1 WHERE id='t'",
            [r#"{"id":"old","id":"t","status":3,"status":"running"}"#],
        )
        .unwrap();
        assert_eq!(
            Table::<TerminalRecord>::new(&tx)
                .get(&"t".to_string())
                .unwrap()
                .unwrap()
                .status(),
            Some("running")
        );
    }
}
record_newtype!(
    UsageResumeRecord,
    String,
    table = "runtime_usage_resumes",
    schema = "PersistedUsageResumeRecord",
    key = External,
    query = UsageResumeRecovery
);
/// Checked operational fields plus the original extensible record. Retaining
/// the original JSON preserves omission, nulls and historical extension fields.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkItemRecord {
    value: Value,
    fields: WorkItemFields,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct WorkItemFields {
    id: TaskId,
    status: WorkStatus,
    // WorkItem.dependencies owns the absent-field default. Present malformed
    // values still fail deserialization, including null.
    #[serde(default)]
    dependencies: Vec<String>,
    owner: Option<String>,
    // Omission retains the existing progress timestamp default. Present null
    // or malformed timestamps cannot become a legitimate epoch-zero value.
    #[serde(default)]
    updated: f64,
}

impl Serialize for WorkItemRecord {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WorkItemRecord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonRecord for WorkItemRecord {
    type Id = TaskId;
    const TABLE: &'static str = "runtime_work";
    const SCHEMA: &'static str = "PersistedWorkItemRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(&self.fields.id)
    }
    fn decode_stored(_connection: &Connection, row_id: &str, encoded: &str) -> StoreResult<Self> {
        Self::decode(row_id, encoded)
    }
}
record_newtype!(
    ExecutionRunRecord,
    String,
    table = "runtime_execution_runs",
    schema = "PersistedExecutionRunRecord",
    key = PayloadId,
    query = None
);
record_newtype!(
    WorkspaceOperationRecord,
    String,
    table = "runtime_workspace_operations",
    schema = "PersistedWorkspaceOperationRecord",
    key = PayloadId,
    query = JsonAgent
);
record_newtype!(
    RuleRecord,
    String,
    table = "runtime_rules",
    schema = "PersistedRuleRecord",
    key = PayloadId,
    query = None
);
record_newtype!(
    TranscriptItemRecord,
    TranscriptItemId,
    table = "runtime_items",
    schema = "PersistedTranscriptRecord",
    key = PayloadId,
    query = PhysicalAgent
);
record_newtype!(
    TranscriptAssetRecord,
    TranscriptAssetId,
    table = "runtime_assets",
    schema = "PersistedTranscriptAsset",
    key = PayloadId,
    query = JsonAgent
);
record_newtype!(
    EventMetadataRecord,
    QueueEventId,
    table = "runtime_event_meta",
    schema = "RuntimeEventMetadataRecord",
    key = External,
    query = None
);
record_newtype!(
    GraphAgentRecord,
    String,
    table = "graph_agents",
    schema = "PersistedGraphAgentRecord",
    key = PayloadId,
    query = None
);

impl WorkItemRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let fields = WorkItemFields::deserialize(&value)?;
        Ok(Self { value, fields })
    }

    pub fn checked(value: Value) -> StoreResult<Self> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_owned();
        Self::checked_at(value, &id)
    }

    fn checked_at(value: Value, row_id: &str) -> StoreResult<Self> {
        #[cfg(test)]
        validation_counts::CHECKS.with(|count| count.set(count.get() + 1));
        if let Some((pointer, reason)) = studio_runtime_schema::first_error(Self::SCHEMA, &value)
            .map_err(crate::StoreError::SchemaValidation)?
        {
            return Err(crate::StoreError::InvalidSyncObjectField {
                scope: Self::TABLE.into(),
                id: row_id.to_owned(),
                pointer,
                reason,
            });
        }
        Self::from_value(value).map_err(|error| crate::StoreError::InvalidSyncObjectField {
            scope: Self::TABLE.into(),
            id: row_id.to_owned(),
            pointer: String::new(),
            reason: error.to_string(),
        })
    }

    pub fn decode(row_id: &str, encoded: &str) -> StoreResult<Self> {
        let invalid = || crate::StoreError::InvalidStoredObject {
            table: Self::TABLE,
            id: row_id.to_owned(),
        };
        let record = Self::checked_at(
            serde_json::from_str(encoded).map_err(|_| invalid())?,
            row_id,
        )?;
        if record.id().as_ref() != row_id {
            return Err(invalid());
        }
        Ok(record)
    }

    pub fn id(&self) -> &TaskId {
        &self.fields.id
    }
    pub fn value(&self) -> &Value {
        &self.value
    }
    pub fn into_value(self) -> Value {
        self.value
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.value.get(key)
    }
    pub fn status(&self) -> WorkStatus {
        self.fields.status
    }
    pub fn dependencies(&self) -> &[String] {
        &self.fields.dependencies
    }
    pub fn owner(&self) -> Option<&str> {
        self.fields.owner.as_deref()
    }
    pub fn updated(&self) -> f64 {
        self.fields.updated
    }

    pub fn blocked_by(&self, statuses: &BTreeMap<String, WorkStatus>) -> Vec<String> {
        self.dependencies()
            .iter()
            .filter(|id| statuses.get(*id) != Some(&WorkStatus::Accepted))
            .cloned()
            .collect()
    }

    pub fn set(&mut self, key: impl Into<String>, value: Value) -> Result<(), serde_json::Error> {
        let mut updated = self.value.clone();
        updated[key.into()] = value;
        let updated = Self::from_value(updated)?;
        if updated.id() != self.id() {
            return Err(<serde_json::Error as serde::de::Error>::custom(
                "work record id is immutable",
            ));
        }
        *self = updated;
        Ok(())
    }

    pub fn title(&self) -> Option<&str> {
        self.value.get("title").and_then(Value::as_str)
    }

    pub fn set_status(&mut self, status: WorkStatus) {
        self.value["status"] = serde_json::json!(status);
        self.fields.status = status;
    }
}

fn record_id(value: &Value) -> Result<String, serde_json::Error> {
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| serde::de::Error::custom("record id must be a string"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordKeyMode {
    PayloadId,
    External,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordQueryShape {
    None,
    JsonAgent,
    PhysicalAgent,
    UsageResumeRecovery,
}

/// The only row filters shared by JSON-record tables.
#[derive(Clone, Copy, Debug)]
pub enum RecordPredicate<'a> {
    PhysicalAgentEquals(&'a str),
    PhysicalAgentIn(&'a [String]),
    JsonAgentEquals(&'a str),
    JsonAgentIn(&'a [String]),
    UsageResumeAuthRecoveryBefore(f64),
}

/// Supported orderings have fixed SQL expressions and tie-breakers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOrder {
    RowId,
    PhysicalCreatedDescIdDesc,
    PhysicalCreatedAscIdAsc,
    JsonCreatedDescIdDesc,
    JsonEffectiveCreatedAscIdAsc,
}

/// A JSON object stored in an id/record table.
pub trait JsonRecord: Serialize + DeserializeOwned {
    type Id: AsRef<str> + From<String>;

    const TABLE: &'static str;
    const SCHEMA: &'static str;
    const KEY_MODE: RecordKeyMode;
    const QUERY_SHAPE: RecordQueryShape;
    const TYPED_SHAPE: bool = false;
    const DIRECT_TYPED_DECODE: bool = false;

    fn record_key(&self) -> Option<&Self::Id>;

    fn decode_stored(connection: &Connection, row_id: &str, encoded: &str) -> StoreResult<Self> {
        decode_record::<Self>(connection, row_id, encoded)
    }
}

/// Typed access to one id/record JSON table.
pub struct Table<'connection, R> {
    connection: &'connection Connection,
    record: PhantomData<fn() -> R>,
}

impl<'connection, R: JsonRecord> Table<'connection, R> {
    pub fn new(connection: &'connection Connection) -> Self {
        Self {
            connection,
            record: PhantomData,
        }
    }

    /// The record text a reader receives: an agent's with its volatile fields.
    fn reader_text(&self, row_id: &str, encoded: String) -> StoreResult<String> {
        if R::TABLE == AgentRecord::TABLE {
            crate::agent_volatile::merged_text(self.connection, row_id, encoded)
        } else {
            Ok(encoded)
        }
    }

    pub(crate) fn decode_stored(&self, row_id: &str, encoded: &str) -> StoreResult<R> {
        R::decode_stored(self.connection, row_id, encoded)
    }

    pub fn get(&self, id: &R::Id) -> StoreResult<Option<R>> {
        self.get_matching(id, None)
    }

    pub fn get_matching(
        &self,
        id: &R::Id,
        predicate: Option<RecordPredicate<'_>>,
    ) -> StoreResult<Option<R>> {
        ensure_query::<R>(predicate, RecordOrder::RowId)?;
        let (where_sql, mut values) = predicate_sql(predicate);
        let id_predicate = where_sql.map_or_else(String::new, |sql| format!(" AND {sql}"));
        values.insert(0, SqlValue::Text(id.as_ref().to_owned()));
        let sql = format!(
            "SELECT record FROM {} WHERE id = ?1{id_predicate}",
            quoted_table::<R>()
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let encoded = stored_text_ref(row.get_ref(0)?, R::TABLE, id.as_ref())?;
        R::decode_stored(self.connection, id.as_ref(), encoded).map(Some)
    }

    /// Keep the original JSON text for consumers whose contract depends on
    /// object property order, such as elicitation schemas.
    pub fn get_with_raw(&self, id: &R::Id) -> StoreResult<Option<(R, String)>> {
        self.get_with_raw_matching(id, None)
    }

    fn get_with_raw_matching(
        &self,
        id: &R::Id,
        predicate: Option<RecordPredicate<'_>>,
    ) -> StoreResult<Option<(R, String)>> {
        ensure_query::<R>(predicate, RecordOrder::RowId)?;
        let (where_sql, mut values) = predicate_sql(predicate);
        let id_predicate = where_sql.map_or_else(String::new, |sql| format!(" AND {sql}"));
        values.insert(0, SqlValue::Text(id.as_ref().to_owned()));
        let sql = format!(
            "SELECT record FROM {} WHERE id = ?1{id_predicate}",
            quoted_table::<R>()
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let encoded = stored_text(row.get_ref(0)?, R::TABLE, id.as_ref())?;
        let record = R::decode_stored(self.connection, id.as_ref(), &encoded)?;
        Ok(Some((record, self.reader_text(id.as_ref(), encoded)?)))
    }

    pub fn list(&self) -> StoreResult<Vec<R>> {
        self.list_matching(None, RecordOrder::RowId)
    }

    pub fn list_matching(
        &self,
        predicate: Option<RecordPredicate<'_>>,
        order: RecordOrder,
    ) -> StoreResult<Vec<R>> {
        ensure_query::<R>(predicate, order)?;
        let (where_sql, values) = predicate_sql(predicate);
        let where_clause = where_sql.map_or_else(String::new, |sql| format!(" WHERE {sql}"));
        let sql = format!(
            "SELECT id,record FROM {}{where_clause} ORDER BY {}",
            quoted_table::<R>(),
            order.sql()
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let id = stored_text(row.get_ref(0)?, R::TABLE, "<invalid id>")?;
            let encoded = stored_text_ref(row.get_ref(1)?, R::TABLE, &id)?;
            records.push(R::decode_stored(self.connection, &id, encoded)?);
        }
        Ok(records)
    }

    pub fn list_with_raw(&self) -> StoreResult<Vec<(R, String)>> {
        self.list_with_raw_matching(None, RecordOrder::RowId)
    }

    pub(crate) fn list_with_raw_matching(
        &self,
        predicate: Option<RecordPredicate<'_>>,
        order: RecordOrder,
    ) -> StoreResult<Vec<(R, String)>> {
        ensure_query::<R>(predicate, order)?;
        let (where_sql, values) = predicate_sql(predicate);
        let where_clause = where_sql.map_or_else(String::new, |sql| format!(" WHERE {sql}"));
        let sql = format!(
            "SELECT id,record FROM {}{where_clause} ORDER BY {}",
            quoted_table::<R>(),
            order.sql()
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let id = stored_text(row.get_ref(0)?, R::TABLE, "<invalid id>")?;
            let encoded = stored_text(row.get_ref(1)?, R::TABLE, &id)?;
            let record = R::decode_stored(self.connection, &id, &encoded)?;
            records.push((record, self.reader_text(&id, encoded)?));
        }
        Ok(records)
    }

    /// Physical-row lookup for change hints; decodes like [`Table::get`].
    pub fn get_by_row_id(&self, row_id: i64) -> StoreResult<Option<R>> {
        ensure_query::<R>(None, RecordOrder::RowId)?;
        let sql = format!(
            "SELECT id,record FROM {} WHERE rowid = ?1",
            quoted_table::<R>()
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let mut rows = statement.query([row_id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let id = stored_text(row.get_ref(0)?, R::TABLE, "<invalid id>")?;
        let encoded = stored_text_ref(row.get_ref(1)?, R::TABLE, &id)?;
        R::decode_stored(self.connection, &id, encoded).map(Some)
    }

    /// Full rowid-ordered read that keeps each physical row id beside its record.
    pub fn list_with_row_ids(&self) -> StoreResult<Vec<(i64, R)>> {
        ensure_query::<R>(None, RecordOrder::RowId)?;
        let sql = format!(
            "SELECT rowid,id,record FROM {} ORDER BY rowid",
            quoted_table::<R>()
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let mut rows = statement.query(())?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let row_id = row.get::<_, i64>(0)?;
            let id = stored_text(row.get_ref(1)?, R::TABLE, "<invalid id>")?;
            let encoded = stored_text_ref(row.get_ref(2)?, R::TABLE, &id)?;
            records.push((row_id, R::decode_stored(self.connection, &id, encoded)?));
        }
        Ok(records)
    }

    pub fn put(&self, record: &R) -> StoreResult<()> {
        if R::KEY_MODE != RecordKeyMode::PayloadId {
            return Err(crate::StoreError::RecordContract(
                "external-key records must be written with put_at",
            ));
        }
        let key = record
            .record_key()
            .ok_or(crate::StoreError::RecordContract(
                "payload-id record has no cached id",
            ))?;
        self.put_at_key(key.as_ref(), record, true).map(|_| ())
    }

    /// Upsert a record and related non-record columns in a hybrid table.
    pub fn put_with_columns(&self, record: &R, columns: &[(&str, SqlValue)]) -> StoreResult<()> {
        if R::KEY_MODE != RecordKeyMode::PayloadId || R::TABLE == AgentRecord::TABLE {
            return Err(crate::StoreError::RecordContract(
                "external-key records must be written with put_at",
            ));
        }
        let key = record
            .record_key()
            .ok_or(crate::StoreError::RecordContract(
                "payload-id record has no cached id",
            ))?;
        let encoded = encode_record(record, key.as_ref())?;
        let mut names = vec!["id".to_owned(), "record".to_owned()];
        let mut placeholders = vec!["?1".to_owned(), "?2".to_owned()];
        let mut updates = vec!["record=excluded.record".to_owned()];
        let mut values = vec![
            SqlValue::Text(key.as_ref().to_owned()),
            SqlValue::Text(encoded),
        ];
        for (index, (name, value)) in columns.iter().enumerate() {
            let mut bytes = name.bytes();
            let valid_identifier = bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
                && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            if !valid_identifier
                || *name == "id"
                || *name == "record"
                || columns[..index].iter().any(|(prior, _)| prior == name)
            {
                return Err(crate::StoreError::RecordContract(
                    "hybrid-table column name is invalid",
                ));
            }
            names.push(format!("\"{name}\""));
            placeholders.push(format!("?{}", index + 3));
            updates.push(format!("\"{name}\"=excluded.\"{name}\""));
            values.push(value.clone());
        }
        let sql = format!(
            "INSERT INTO {}({}) VALUES ({}) ON CONFLICT(id) DO UPDATE SET {}",
            quoted_table::<R>(),
            names.join(","),
            placeholders.join(","),
            updates.join(","),
        );
        self.connection
            .execute(&sql, params_from_iter(values.iter()))?;
        Ok(())
    }

    pub fn validate(&self, record: &R) -> StoreResult<()> {
        if R::KEY_MODE != RecordKeyMode::PayloadId {
            return Err(crate::StoreError::RecordContract(
                "external-key records must be validated with validate_at",
            ));
        }
        let key = record
            .record_key()
            .ok_or(crate::StoreError::RecordContract(
                "payload-id record has no cached id",
            ))?;
        checked_value(record, key.as_ref()).map(|_| ())
    }

    pub fn validate_at(&self, id: &R::Id, record: &R) -> StoreResult<()> {
        if R::KEY_MODE != RecordKeyMode::External {
            return Err(crate::StoreError::RecordContract(
                "validate_at is only valid for external-key records",
            ));
        }
        checked_value(record, id.as_ref()).map(|_| ())
    }

    /// Upsert a payload without adding the external table key to its JSON.
    pub fn put_at(&self, id: &R::Id, record: &R) -> StoreResult<()> {
        if R::KEY_MODE != RecordKeyMode::External {
            return Err(crate::StoreError::RecordContract(
                "put_at is only valid for external-key records",
            ));
        }
        self.put_at_key(id.as_ref(), record, true).map(|_| ())
    }

    pub fn update(&self, record: &R) -> StoreResult<bool> {
        if R::KEY_MODE != RecordKeyMode::PayloadId {
            return Err(crate::StoreError::RecordContract(
                "external-key records must be updated with update_at",
            ));
        }
        let key = record
            .record_key()
            .ok_or(crate::StoreError::RecordContract(
                "payload-id record has no cached id",
            ))?;
        self.update_at_key(key.as_ref(), record)
    }

    pub fn update_at(&self, id: &R::Id, record: &R) -> StoreResult<bool> {
        if R::KEY_MODE != RecordKeyMode::External {
            return Err(crate::StoreError::RecordContract(
                "update_at is only valid for external-key records",
            ));
        }
        self.update_at_key(id.as_ref(), record)
    }

    fn update_at_key(&self, id: &str, record: &R) -> StoreResult<bool> {
        if R::TABLE == AgentRecord::TABLE {
            let value = checked_value(record, id)?;
            return crate::agent_volatile::update(self.connection, id, &value);
        }
        let encoded = encode_record(record, id)?;
        let changed = self.connection.execute(
            &format!("UPDATE {} SET record=?1 WHERE id=?2", quoted_table::<R>()),
            params![encoded, id],
        )?;
        Ok(changed == 1)
    }

    fn put_at_key(&self, id: &str, record: &R, update: bool) -> StoreResult<bool> {
        if R::TABLE == AgentRecord::TABLE {
            let value = checked_value(record, id)?;
            return crate::agent_volatile::put(self.connection, id, &value, !update);
        }
        let encoded = encode_record(record, id)?;
        let conflict = if update {
            "DO UPDATE SET record=excluded.record"
        } else {
            "DO NOTHING"
        };
        let sql = format!(
            "INSERT INTO {}(id,record) VALUES (?1,?2) ON CONFLICT(id) {conflict}",
            quoted_table::<R>()
        );
        let changed = self.connection.execute(&sql, params![id, encoded])?;
        Ok(changed == 1)
    }

    pub fn insert_new(&self, record: &R) -> StoreResult<bool> {
        if R::KEY_MODE != RecordKeyMode::PayloadId {
            return Err(crate::StoreError::RecordContract(
                "external-key records must be written with insert_new_at",
            ));
        }
        let key = record
            .record_key()
            .ok_or(crate::StoreError::RecordContract(
                "payload-id record has no cached id",
            ))?;
        self.put_at_key(key.as_ref(), record, false)
    }

    pub fn insert_new_at(&self, id: &R::Id, record: &R) -> StoreResult<bool> {
        if R::KEY_MODE != RecordKeyMode::External {
            return Err(crate::StoreError::RecordContract(
                "insert_new_at is only valid for external-key records",
            ));
        }
        self.put_at_key(id.as_ref(), record, false)
    }

    pub fn delete(&self, id: &R::Id) -> StoreResult<bool> {
        let changed = self.connection.execute(
            &format!("DELETE FROM {} WHERE id=?1", quoted_table::<R>()),
            [id.as_ref()],
        )?;
        Ok(changed == 1)
    }
}

impl RecordOrder {
    fn sql(self) -> &'static str {
        match self {
            Self::RowId => "rowid",
            Self::PhysicalCreatedDescIdDesc => "created DESC,id DESC",
            Self::PhysicalCreatedAscIdAsc => "created ASC,id ASC",
            Self::JsonCreatedDescIdDesc => "json_extract(record,'$.created') DESC,id DESC",
            Self::JsonEffectiveCreatedAscIdAsc => {
                "CASE WHEN COALESCE(json_extract(record,'$.finished'),0) > \
                 COALESCE(json_extract(record,'$.created'),0) \
                 THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END ASC,id ASC"
            }
        }
    }
}

fn ensure_query<R: JsonRecord>(
    predicate: Option<RecordPredicate<'_>>,
    order: RecordOrder,
) -> StoreResult<()> {
    let valid_filter = match predicate {
        None => true,
        Some(RecordPredicate::PhysicalAgentEquals(_) | RecordPredicate::PhysicalAgentIn(_)) => {
            R::QUERY_SHAPE == RecordQueryShape::PhysicalAgent
        }
        Some(RecordPredicate::JsonAgentEquals(_) | RecordPredicate::JsonAgentIn(_)) => {
            R::QUERY_SHAPE == RecordQueryShape::JsonAgent
        }
        Some(RecordPredicate::UsageResumeAuthRecoveryBefore(_)) => {
            R::QUERY_SHAPE == RecordQueryShape::UsageResumeRecovery
        }
    };
    let valid_order = match order {
        RecordOrder::RowId => true,
        RecordOrder::PhysicalCreatedDescIdDesc | RecordOrder::PhysicalCreatedAscIdAsc => {
            R::QUERY_SHAPE == RecordQueryShape::PhysicalAgent
        }
        RecordOrder::JsonCreatedDescIdDesc | RecordOrder::JsonEffectiveCreatedAscIdAsc => {
            R::QUERY_SHAPE == RecordQueryShape::JsonAgent
        }
    };
    if valid_filter && valid_order {
        Ok(())
    } else {
        Err(crate::StoreError::RecordContract(
            "query choice is not declared by this record type",
        ))
    }
}

fn predicate_sql(predicate: Option<RecordPredicate<'_>>) -> (Option<String>, Vec<SqlValue>) {
    match predicate {
        None => (None, Vec::new()),
        Some(RecordPredicate::PhysicalAgentEquals(value)) => (
            Some("agent=?".to_owned()),
            vec![SqlValue::Text(value.to_owned())],
        ),
        Some(RecordPredicate::JsonAgentEquals(value)) => (
            Some("json_extract(record,'$.agent')=?".to_owned()),
            vec![SqlValue::Text(value.to_owned())],
        ),
        Some(RecordPredicate::PhysicalAgentIn(values)) => in_predicate("agent", values),
        Some(RecordPredicate::JsonAgentIn(values)) => {
            in_predicate("json_extract(record,'$.agent')", values)
        }
        Some(RecordPredicate::UsageResumeAuthRecoveryBefore(failed_before)) => (
            Some(
                concat!(
                    "CASE WHEN json_valid(record) THEN ",
                    "json_extract(record,'$.status')='scheduled' ",
                    "AND json_extract(record,'$.cause')='auth' ",
                    "AND json_extract(record,'$.failedAt')<=?1 ELSE 0 END"
                )
                .to_owned(),
            ),
            vec![SqlValue::Real(failed_before)],
        ),
    }
}

fn in_predicate(column: &str, values: &[String]) -> (Option<String>, Vec<SqlValue>) {
    if values.is_empty() {
        return (Some("0".to_owned()), Vec::new());
    }
    let marks = vec!["?"; values.len()].join(",");
    (
        Some(format!("{column} IN ({marks})")),
        values.iter().cloned().map(SqlValue::Text).collect(),
    )
}

fn quoted_table<R: JsonRecord>() -> String {
    format!("\"{}\"", R::TABLE.replace('"', "\"\""))
}

pub(crate) fn validate_value(
    schema: &str,
    value: Value,
    invalid: impl Fn() -> crate::StoreError,
) -> StoreResult<Value> {
    if !studio_runtime_schema::validate(schema, &value)
        .map_err(crate::StoreError::SchemaValidation)?
    {
        return Err(invalid());
    }
    Ok(value)
}

pub(crate) fn decode_value(
    schema: &str,
    encoded: &str,
    invalid: impl Fn() -> crate::StoreError,
) -> StoreResult<Value> {
    let value = serde_json::from_str(encoded).map_err(|_| invalid())?;
    validate_value(schema, value, invalid)
}

pub(crate) fn decode_record<R: JsonRecord>(
    connection: &Connection,
    row_id: &str,
    encoded: &str,
) -> StoreResult<R> {
    if R::TABLE == AgentRecord::TABLE {
        // The checked value already carries the typed fields; decode moves it.
        return serde_json::from_value::<R>(decode_agent_value(connection, row_id, encoded)?)
            .map_err(|_| invalid_agent_field(row_id, "", "record_decode", None));
    }
    let invalid = || crate::StoreError::InvalidStoredObject {
        table: R::TABLE,
        id: row_id.to_owned(),
    };
    let record = if R::DIRECT_TYPED_DECODE {
        serde_json::from_str::<R>(encoded).map_err(|_| invalid())?
    } else if R::TYPED_SHAPE {
        // Stored JSON was historically parsed to a map first, so duplicate
        // property names retain serde_json's last-value behavior.
        let value = serde_json::from_str::<Value>(encoded).map_err(|_| invalid())?;
        serde_json::from_value::<R>(value).map_err(|_| invalid())?
    } else {
        let value = decode_value(R::SCHEMA, encoded, invalid)?;
        serde_json::from_value::<R>(value).map_err(|_| invalid())?
    };
    if R::KEY_MODE == RecordKeyMode::PayloadId
        && record.record_key().map(AsRef::as_ref) != Some(row_id)
    {
        return Err(invalid());
    }
    Ok(record)
}

/// An agent row as checked JSON with its volatile fields merged
/// ([`crate::agent_volatile::merge`]): the schema, the [`AgentRecord`] typed
/// fields and the row key, without building the typed record.
pub fn decode_agent_value(
    connection: &Connection,
    row_id: &str,
    encoded: &str,
) -> StoreResult<Value> {
    let mut value = parse_agent_value(row_id, encoded)?;
    crate::agent_volatile::merge(connection, row_id, &mut value)?;
    validate_agent_value(row_id, value)
}

pub(crate) fn parse_agent_value(row_id: &str, encoded: &str) -> StoreResult<Value> {
    #[cfg(any(test, feature = "test-support"))]
    crate::agent_volatile::publication_work::record(
        row_id,
        crate::agent_volatile::publication_work::Operation::Decode,
    );
    let value: Value = serde_json::from_str(encoded).map_err(|_| {
        invalid_agent_field(
            row_id,
            "",
            &format!("invalid_json; record length {} bytes", encoded.len()),
            None,
        )
    })?;
    Ok(value)
}

pub(crate) fn validate_agent_value(row_id: &str, value: Value) -> StoreResult<Value> {
    /// The fields [`AgentRecord`] types; every other field is free JSON.
    #[derive(Deserialize)]
    struct TypedFields<'a> {
        id: &'a str,
        #[serde(rename = "rootId")]
        _root_id: &'a str,
        #[serde(rename = "epoch")]
        _epoch: i64,
        #[serde(rename = "status")]
        _status: &'a str,
    }
    if !studio_runtime_schema::validate(AgentRecord::SCHEMA, &value)
        .map_err(crate::StoreError::SchemaValidation)?
    {
        let (pointer, reason) = studio_runtime_schema::first_error(AgentRecord::SCHEMA, &value)
            .map_err(crate::StoreError::SchemaValidation)?
            .unwrap_or_else(|| ("/".to_owned(), "schema_invalid".to_owned()));
        return Err(invalid_agent_field(row_id, &pointer, &reason, Some(&value)));
    }
    let fields = TypedFields::deserialize(&value)
        .map_err(|_| invalid_agent_field(row_id, "", "record_decode", None))?;
    if fields.id != row_id {
        return Err(invalid_agent_field(
            row_id,
            "/id",
            "row_id_mismatch",
            Some(&value),
        ));
    }
    Ok(value)
}

pub(crate) fn invalid_agent_field(
    row_id: &str,
    pointer: &str,
    reason: &str,
    record: Option<&Value>,
) -> crate::StoreError {
    let value = record.and_then(|record| record.pointer(pointer));
    let (value_type, length) = match value {
        Some(Value::String(value)) => ("string", format!("{} bytes", value.len())),
        Some(Value::Array(value)) => ("array", format!("{} items", value.len())),
        Some(Value::Object(value)) => ("object", format!("{} properties", value.len())),
        Some(Value::Number(_)) => ("number", "not applicable".to_owned()),
        Some(Value::Bool(_)) => ("boolean", "not applicable".to_owned()),
        Some(Value::Null) => ("null", "not applicable".to_owned()),
        None => ("missing", "not applicable".to_owned()),
    };
    crate::StoreError::InvalidSyncObjectField {
        scope: "runtime_agents".to_owned(),
        id: row_id.to_owned(),
        pointer: pointer.to_owned(),
        reason: format!("{reason}; value type {value_type}, length {length}"),
    }
}

fn encode_record<R: JsonRecord>(record: &R, row_id: &str) -> StoreResult<String> {
    #[cfg(test)]
    validation_counts::ENCODINGS.with(|count| count.set(count.get() + 1));
    Ok(serde_json::to_string(&checked_value(record, row_id)?)?)
}

/// The record as JSON, checked against its schema and row key.
fn checked_value<R: JsonRecord>(record: &R, row_id: &str) -> StoreResult<Value> {
    #[cfg(test)]
    validation_counts::CHECKS.with(|count| count.set(count.get() + 1));
    let value = serde_json::to_value(record)?;
    let invalid = || crate::StoreError::InvalidStoredObject {
        table: R::TABLE,
        id: row_id.to_owned(),
    };
    if (R::KEY_MODE == RecordKeyMode::PayloadId
        && (record.record_key().map(AsRef::as_ref) != Some(row_id)
            || value.get("id").and_then(Value::as_str) != Some(row_id)))
        || (!R::TYPED_SHAPE
            && !studio_runtime_schema::validate(R::SCHEMA, &value)
                .map_err(crate::StoreError::SchemaValidation)?)
    {
        return Err(invalid());
    }
    Ok(value)
}

fn stored_text(value: ValueRef<'_>, table: &'static str, id: &str) -> StoreResult<String> {
    match value {
        ValueRef::Text(bytes) => {
            String::from_utf8(bytes.to_vec()).map_err(|_| crate::StoreError::InvalidStoredObject {
                table,
                id: id.to_owned(),
            })
        }
        ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => {
            Err(crate::StoreError::InvalidStoredObject {
                table,
                id: id.to_owned(),
            })
        }
    }
}

fn stored_text_ref<'row>(
    value: ValueRef<'row>,
    table: &'static str,
    id: &str,
) -> StoreResult<&'row str> {
    match value {
        ValueRef::Text(bytes) => {
            std::str::from_utf8(bytes).map_err(|_| crate::StoreError::InvalidStoredObject {
                table,
                id: id.to_owned(),
            })
        }
        ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => {
            Err(crate::StoreError::InvalidStoredObject {
                table,
                id: id.to_owned(),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueEvent {
    pub id: QueueEventId,
    pub agent: AgentId,
    pub kind: String,
    pub text: String,
    pub status: String,
    pub created: f64,
    pub epoch: i64,
    pub turn_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub sequence: i64,
    pub id: ChatMessageId,
    pub room: String,
    pub sender: String,
    pub text: String,
    pub created: f64,
    pub deliveries: Value,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AgentRepository;

#[derive(Clone, Copy, Debug, Default)]
pub struct TaskRepository;

#[derive(Clone, Copy, Debug, Default)]
pub struct QueueEventRepository;

#[derive(Clone, Copy, Debug, Default)]
pub struct MessageRepository;

pub(crate) fn get_json_record<T: DeserializeOwned>(
    connection: &Connection,
    table: &str,
    id: &str,
) -> StoreResult<Option<T>> {
    connection
        .query_row(
            &format!("SELECT record FROM {table} WHERE id = ?1"),
            [id],
            |row| decode_json_record(row, 0),
        )
        .optional()
        .map_err(Into::into)
}

pub(crate) fn decode_json_record<T: DeserializeOwned>(
    row: &Row<'_>,
    column: usize,
) -> rusqlite::Result<T> {
    let json: String = row.get(column)?;
    serde_json::from_str(&json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AgentId, AgentRecord, AgentStatus, EventMetadataRecord, QueueEventId, RecordOrder,
        RecordPredicate, RequestRecord, Table, TaskRecord, TranscriptAssetId,
        TranscriptAssetRecord, TranscriptItemId, TranscriptItemRecord, UnknownAgentStatus,
        WorkItemRecord, WorkStatus,
    };
    use crate::StoreError;
    use rusqlite::Connection;
    use serde_json::{Value, json};

    #[test]
    fn agent_effect_records_keep_live_and_current_shapes() {
        let legacy = r#"{"id":"agent","rootId":"agent","epoch":1,"status":"idle","nativeReviewThread":{"attempt":"attempt","connection":"connection","generation":2,"operation":"operation","requestDigest":"digest","sequence":1,"status":"accepted","threadId":"thread"},"nativeRelease":{"id":"release","phase":"released","resetPending":false}}"#;
        let agent: AgentRecord = serde_json::from_str(legacy).expect("live review and release");
        assert!(
            agent
                .native_review_thread
                .as_ref()
                .unwrap()
                .root_request_id
                .is_none()
        );
        assert!(agent.native_release.as_ref().unwrap().scope.is_none());
        let encoded = serde_json::to_string(&agent).expect("serialize legacy agent");
        let round_trip: AgentRecord = serde_json::from_str(&encoded).expect("round-trip agent");
        assert_eq!(agent, round_trip);
        assert_eq!(
            agent.value(),
            serde_json::from_str::<Value>(&encoded).unwrap()
        );
        assert_eq!(agent.clone().into_value(), agent.value());

        let current = r#"{"id":"agent","rootId":"agent","epoch":1,"status":"idle","vmLaunch":{"identity":{"agentId":"agent","epoch":1,"handle":"handle","requestId":"request","launchFingerprint":"digest"},"signature":"signature","status":"submitted"},"credentialCopy":{"agentId":"agent","epoch":1,"accountKey":"account","provider":"codex","relativePath":"path","digest":"digest","requestId":"request","signature":"signature","status":"submitted"},"vmAuthBootstrap":{"agentId":"agent","epoch":1,"requestId":"request","rootRequestId":"root","admittedRequest":{"id":"token","signature":"signature","created":1},"fingerprint":"digest","state":"submitted"},"nativeRelease":{"scope":{"account":"account","connection":"connection","generation":2},"admittedRequest":{"id":"token","signature":"signature","created":1},"id":"release"}}"#;
        let agent: AgentRecord = serde_json::from_str(current).expect("current effect fields");
        let encoded = serde_json::to_string(&agent).expect("serialize current agent");
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap(),
            serde_json::from_str::<Value>(current).unwrap()
        );
        assert_eq!(
            agent.value(),
            serde_json::from_str::<Value>(&encoded).unwrap()
        );
    }

    fn agent(value: Value) -> AgentRecord {
        AgentRecord::from_value(value).expect("agent record has a string id")
    }

    #[test]
    fn start_attempt_view_and_field_writes_preserve_existing_payloads() {
        // Shapes from the execution migration and the scheduler's claim writer.
        for attempt in [
            json!({"id":"attempt-0"}),
            json!({"id":"attempt","epoch":1,"events":["event"],"submitted":false,
                "activeAtReservation":false,"accountKey":"default","created":1.0,
                "nativeRequest":{"params":{"vendorExtension":[1,2]}}}),
        ] {
            let mut record = agent(
                json!({"id":"agent","rootId":"agent","epoch":1,"status":"idle","startAttempt":attempt}),
            );
            let before = record.extra.get("startAttempt").cloned().expect("attempt");
            let view = record.start_attempt().expect("view").expect("attempt");
            assert!(view.id.is_some());
            assert_eq!(record.extra.get("startAttempt"), Some(&before));
            record
                .set_start_attempt_submitted(false)
                .expect("field write");
            let mut expected = before;
            expected["submitted"] = json!(false);
            assert_eq!(record.extra.get("startAttempt"), Some(&expected));
        }
    }

    #[test]
    fn start_attempt_action_view_rejects_unnormalized_values() {
        for action in ["compact", "review", "capacity", "safety"] {
            let record = agent(
                json!({"id":"agent","rootId":"agent","epoch":1,"status":"idle","startAttempt":{"action":action}}),
            );
            assert!(
                record
                    .start_attempt()
                    .expect("valid view")
                    .expect("attempt")
                    .action
                    .is_some()
            );
        }
        for action in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            let record = agent(
                json!({"id":"agent","rootId":"agent","epoch":1,"status":"idle","startAttempt":{"action":action}}),
            );
            assert!(record.start_attempt().is_err());
        }
        for action in [
            json!(true),
            json!(1),
            json!("other"),
            json!([1]),
            json!({"x":1}),
        ] {
            let record = agent(
                json!({"id":"agent","rootId":"agent","epoch":1,"status":"idle","startAttempt":{"action":action}}),
            );
            assert!(record.start_attempt().is_err());
        }
    }

    #[test]
    fn active_execution_view_and_field_writes_preserve_migration_record() {
        let mut record = agent(
            json!({"id":"agent","rootId":"agent","epoch":1,"status":"idle",
            "activeExecution":{"runId":"run","attemptId":null,"futureField":{"keep":true}}}),
        );
        let view = record
            .active_execution()
            .expect("pointer")
            .expect("present");
        assert_eq!(view.run_id.as_deref(), Some("run"));
        assert_eq!(view.attempt_id, None);
        record
            .set_active_execution_attempt_id(Some("attempt-0"))
            .expect("set attempt");
        assert_eq!(
            record.extra["activeExecution"],
            json!({"runId":"run","attemptId":"attempt-0","futureField":{"keep":true}})
        );
        record
            .set_active_execution_attempt_id(None)
            .expect("restore null");
        assert_eq!(
            record.extra["activeExecution"],
            json!({"runId":"run","attemptId":null,"futureField":{"keep":true}})
        );
    }

    #[test]
    fn consuming_agent_projection_moves_extra_json() {
        let record = agent(json!({
            "id":"agent-1", "provider":"codex", "rootId":"root-1", "epoch":3, "status":"running",
            "futureField":[{"nested":"retained"}]
        }));
        let source_ptr = record.extra["futureField"]
            .as_array()
            .expect("extra field is an array")
            .as_ptr();
        let projected = record.into_value();

        assert_eq!(
            projected,
            json!({
                "id":"agent-1", "provider":"codex", "rootId":"root-1", "epoch":3, "status":"running",
                "futureField":[{"nested":"retained"}]
            })
        );
        assert_eq!(
            projected["futureField"]
                .as_array()
                .expect("projected extra field is an array")
                .as_ptr(),
            source_ptr,
            "the consuming projection must move, not clone, the nested JSON tree"
        );
    }

    #[test]
    fn consuming_json_record_projection_moves_value_tree() {
        let record = TranscriptItemRecord::from_value(json!({
            "id":"item-1", "agent":"agent-1", "nested":[{"retained":true}]
        }))
        .expect("transcript item has an id");
        let source_ptr = record.value()["nested"]
            .as_array()
            .expect("nested value is an array")
            .as_ptr();

        let projected = record.into_value();

        assert_eq!(projected["nested"][0]["retained"], Value::Bool(true));
        assert_eq!(
            projected["nested"]
                .as_array()
                .expect("projected nested value is an array")
                .as_ptr(),
            source_ptr,
            "the consuming projection must move, not clone, the record tree"
        );
    }

    fn task(value: Value) -> TaskRecord {
        TaskRecord::from_value(value).expect("runtime task has a string id")
    }

    fn work(value: Value) -> WorkItemRecord {
        WorkItemRecord::from_value(value).expect("work item has a string id")
    }

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("agent table creates");
        connection
            .execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        connection
    }

    #[test]
    fn agent_round_trip_keeps_unknown_json_fields_and_number_kinds() {
        let value = json!({
            "id":"agent-1", "provider":"codex", "rootId":"agent-1", "epoch":2, "status":"idle",
            "futureObject":{"number":1.5,"flag":true}, "futureInteger":9007199254740993_i64
        });
        let record = agent(value.clone());
        let rewritten = serde_json::to_value(record).expect("agent record serializes");
        assert_eq!(rewritten["futureObject"]["number"].as_f64(), Some(1.5));
        assert_eq!(rewritten["futureObject"]["flag"].as_bool(), Some(true));
        assert_eq!(rewritten["futureInteger"].as_i64(), Some(9007199254740993));
        assert_eq!(rewritten, value);
    }

    #[test]
    fn agent_round_trip_keeps_legacy_private_fields() {
        for private in [
            json!({"accountKey":null,"threadId":null,"titleJob":{"legacy":true}}),
            json!({"startOutcomeHold":{"stage":"held","reason":"pending"}}),
            json!({"startOutcomeHold":{"stage":"account_settlement","cursor":null,"reason":"pending"}}),
            json!({"startOutcomeHold":{"stage":"held","operationId":"op","future":1}}),
            json!({"startOutcomeHold":{"stage":"recovery","epoch":2,"reason":"pending"}}),
            json!({"titleJob":{"requestId":"r","agentId":"agent","rootRequestId":"root","admittedRequest":{"id":"r","signature":"s","created":1},"status":"pending","future":true}}),
        ] {
            let mut value = json!({"id":"agent","rootId":"agent","epoch":1,"status":"idle"});
            value
                .as_object_mut()
                .unwrap()
                .extend(private.as_object().unwrap().clone());
            let record = agent(value.clone());
            assert_eq!(crate::projection_value(&record), value);
        }
    }

    #[test]
    fn stored_review_boundary_work_diagnostic_uses_physical_row_id() {
        let connection = Connection::open_in_memory().expect("fixture database");
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .expect("work table");
        connection
            .execute(
                "INSERT INTO runtime_work VALUES('physical-work',?1)",
                [json!({"id":"payload-work","status":null}).to_string()],
            )
            .expect("corrupt fixture");
        assert!(
            matches!(super::Table::<WorkItemRecord>::new(&connection).get(&super::TaskId::from("physical-work")),Err(crate::StoreError::InvalidSyncObjectField{id,..}) if id=="physical-work")
        );
    }

    #[test]
    fn stored_work_boundary_contract_matches_openapi_owner() {
        let openapi: Value = serde_json::from_str(include_str!("../../runtime-http/openapi.json"))
            .expect("OpenAPI owner");
        assert_eq!(
            openapi["components"]["schemas"]["WorkItem"]["properties"]["status"]["$ref"],
            "#/components/schemas/WorkStatus",
            "work status consumes the generated Rust owner"
        );
        let schema: Value = serde_json::from_str(include_str!(
            "../../runtime-schema/src/schemas/persisted-work-item-record.schema.json"
        ))
        .expect("persisted owner");
        for field in ["status", "dependencies", "updated"] {
            assert_eq!(
                schema["properties"][field]["$ref"],
                json!(format!(
                    "{}#/components/schemas/WorkItem/properties/{field}",
                    studio_runtime_schema::OPENAPI_URI
                ))
            );
        }
        assert_eq!(
            openapi["components"]["schemas"]["WorkItem"]["properties"]["dependencies"]["default"],
            json!([])
        );
        let raw = json!({"id":"work","status":"ready","owner":null,"futureField":{"proof":7}});
        let record = WorkItemRecord::checked(raw.clone())
            .expect("nullable owner and absent default dependencies");
        assert_eq!(record.status(), WorkStatus::Ready);
        assert_eq!(record.owner(), None);
        assert!(record.dependencies().is_empty());
        assert_eq!(
            record.updated(),
            0.0,
            "omission keeps the existing progress timestamp default"
        );
        assert_eq!(
            record.into_value(),
            raw,
            "extensions, null and omission remain exact"
        );
    }

    #[test]
    fn runtime_task_and_work_item_use_distinct_exported_schemas() {
        let runtime_task = task(json!({
            "id":"task-1", "agent":"agent-1", "kind":"command", "status":"completed",
            "created":1.25, "interactive":true, "exitCode":7
        }));
        let rewritten = serde_json::to_value(runtime_task).expect("runtime task serializes");
        assert_eq!(rewritten["interactive"], json!(true));
        assert_eq!(rewritten["exitCode"].as_i64(), Some(7));

        let work = work(json!({
            "id":"work-1", "rootId":"agent-1", "status":"ready", "title":"T",
            "created":1.25, "updated":1.5, "version":3, "futureField":true
        }));
        assert_eq!(work.status(), WorkStatus::Ready);
        assert_eq!(
            serde_json::to_value(work).expect("work item serializes")["futureField"],
            json!(true)
        );
    }

    #[test]
    fn persisted_task_and_request_records_accept_python_written_fields() {
        let task = task(json!({
            "id":"task-1", "reattachRecovery":{"status":"running"},
            "outputPreviewOnly":true
        }));
        assert_eq!(
            task.get("reattachRecovery").expect("recovery metadata")["status"],
            "running"
        );
        assert_eq!(task.get("outputPreviewOnly"), Some(&Value::Bool(true)));

        let request = RequestRecord::from_value(json!({
            "id":"request-1", "connectionId":null
        }))
        .expect("Python may persist a request without a connection identity");
        assert_eq!(request.get("connectionId"), Some(&Value::Null));
    }

    #[test]
    fn unknown_agent_status_and_fields_survive_carrier_rewrite() {
        let value = json!({"id":"agent-1","provider":"codex","rootId":"agent-1","epoch":4,
            "status":"future-agent-state","future":{"enabled":true,"count":9007199254740993_i64}});
        let record = agent(value.clone());
        assert_eq!(
            record.status,
            AgentStatus::Other(UnknownAgentStatus::new("future-agent-state".into()).unwrap())
        );
        assert_eq!(
            serde_json::to_value(record).expect("record serializes"),
            value
        );
    }

    #[test]
    fn known_status_has_only_one_representation() {
        for known in AgentStatus::VALUES {
            let decoded: AgentStatus = serde_json::from_str(&format!("{known:?}")).unwrap();
            assert!(UnknownAgentStatus::new((*known).to_owned()).is_none());
            assert_eq!(decoded.as_str(), *known);
        }
        assert_eq!(
            serde_json::from_str::<AgentStatus>("\"running\"").unwrap(),
            AgentStatus::Running
        );
    }

    #[test]
    fn table_upserts_reads_lists_and_inserts_only_once() {
        let connection = database();
        let table = Table::<AgentRecord>::new(&connection);
        let first =
            agent(json!({"id":"a1","provider":"codex","rootId":"a1","epoch":0,"status":"idle"}));
        let second =
            agent(json!({"id":"a2","provider":"codex","rootId":"a2","epoch":0,"status":"queued"}));

        assert!(table.insert_new(&first).expect("first insert succeeds"));
        assert!(!table.insert_new(&first).expect("duplicate id is ignored"));
        assert!(table.insert_new(&second).expect("second insert succeeds"));
        assert_eq!(
            table
                .get(&AgentId::from("a2"))
                .expect("agent reads")
                .expect("agent exists")
                .id(),
            &AgentId::from("a2")
        );

        let mut changed = first;
        changed.set_status(AgentStatus::Running);
        table
            .put(&changed)
            .expect("put updates the existing record");
        assert_eq!(
            table.list().expect("records list")[0].status,
            AgentStatus::Running
        );
    }

    #[test]
    fn table_refuses_invalid_json_and_row_id_mismatches() {
        let connection = database();
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('bad-json','{')",
                [],
            )
            .expect("malformed fixture inserts");
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('outer-id','{\"id\":\"inner-id\",\"rootId\":\"inner-id\",\"epoch\":0,\"status\":\"idle\"}')",
                [],
            )
            .expect("mismatched fixture inserts");
        let table = Table::<AgentRecord>::new(&connection);

        for id in ["bad-json", "outer-id"] {
            let error = table
                .get(&AgentId::from(id))
                .expect_err("invalid record is refused");
            assert!(error.to_string().contains(&format!("runtime_agents/{id}")));
            if id == "outer-id" {
                assert!(error.to_string().contains("/id"));
                assert!(error.to_string().contains("row_id_mismatch"));
            } else {
                assert!(error.to_string().contains("invalid_json"));
                assert!(!error.to_string().contains("{\"id\""));
            }
        }
    }

    #[test]
    fn non_text_and_invalid_utf8_records_are_invalid_stored_objects() {
        let connection = database();
        connection
            .execute_batch(
                "INSERT INTO runtime_agents(id,record) VALUES('blob',x'7b7d');
                 INSERT INTO runtime_agents(id,record) VALUES('invalid-utf8',CAST(x'ff' AS TEXT));",
            )
            .expect("corrupt record fixtures insert");
        let table = Table::<AgentRecord>::new(&connection);

        for id in ["blob", "invalid-utf8"] {
            assert!(matches!(
                table.get(&AgentId::from(id)).expect_err("bad storage type is rejected"),
                StoreError::InvalidStoredObject { table: "runtime_agents", id: ref bad_id }
                    if bad_id == id
            ));
        }
        assert!(matches!(
            table.list().expect_err("bad list row is rejected"),
            StoreError::InvalidStoredObject { table: "runtime_agents", id }
                if id == "blob"
        ));
    }

    #[test]
    fn invalid_schema_reads_and_writes_include_the_record_key() {
        let connection = database();
        let table = Table::<AgentRecord>::new(&connection);
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('invalid-epoch','{\"id\":\"invalid-epoch\",\"rootId\":[\"private user text\"],\"epoch\":0,\"status\":\"idle\"}')",
                [],
            )
            .expect("invalid record fixture inserts");
        let error = table
            .list()
            .expect_err("schema-invalid record is not returned");
        assert!(
            matches!(error, StoreError::InvalidSyncObjectField { ref scope, ref id, ref pointer, .. }
            if scope == "runtime_agents" && id == "invalid-epoch" && pointer == "/rootId")
        );
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("value type array"));
        assert!(diagnostic.contains("length 1 items"));
        assert!(!diagnostic.contains("private user text"));
        let mut invalid = agent(
            json!({"id":"write","provider":"codex","rootId":"write","epoch":0,"status":"idle"}),
        );
        invalid
            .extra
            .insert("autoWake".into(), json!("private user text"));
        let before = connection.total_changes();
        let error = table
            .put(&invalid)
            .expect_err("invalid typed write is rejected");
        assert!(
            matches!(error, StoreError::InvalidStoredObject { table: "runtime_agents", ref id } if id == "write")
        );
        assert!(!error.to_string().contains("private user text"));
        assert_eq!(
            connection.total_changes(),
            before,
            "invalid write changes no rows"
        );
    }

    #[test]
    fn external_keys_do_not_become_json_fields() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
            )
            .expect("optional event metadata table creates");
        let table = Table::<EventMetadataRecord>::new(&connection);
        let key = QueueEventId::from("event-1");
        let value = EventMetadataRecord::from_value(json!({"timing":{"enqueuedAt":9}}))
            .expect("metadata is an object");
        assert!(
            table
                .insert_new_at(&key, &value)
                .expect("first keyed insert")
        );
        assert!(
            !table
                .insert_new_at(&key, &value)
                .expect("duplicate keyed insert")
        );
        assert_eq!(
            table
                .get(&key)
                .expect("external key read")
                .expect("metadata exists")
                .value(),
            &json!({"timing":{"enqueuedAt":9}})
        );
        let stored: String = connection
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id='event-1'",
                [],
                |row| row.get(0),
            )
            .expect("record reads");
        let value = serde_json::from_str::<Value>(&stored).expect("stored metadata parses");
        assert_eq!(value, json!({"timing":{"enqueuedAt":9}}));
        assert!(value.get("id").is_none());
    }

    #[test]
    fn persisted_asset_table_keeps_path_and_extension_fields() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        let table = Table::<TranscriptAssetRecord>::new(&connection);
        connection
            .execute_batch("CREATE TABLE runtime_assets(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("assets table creates");

        let asset = TranscriptAssetRecord::from_value(json!({
            "id":"asset-1", "agent":"agent-1", "name":"notes.txt",
            "path":"/private/uploads/asset-1/notes.txt", "mime":"text/plain",
            "image":false, "size":5, "hash":"sha256:abc", "created":1.5,
            "futureField":{"retained":true}
        }))
        .expect("asset has an id");
        table
            .put(&asset)
            .expect("persisted asset validates and writes");

        let read = table
            .get(&TranscriptAssetId::from("asset-1"))
            .expect("asset reads")
            .expect("asset exists");
        assert_eq!(
            read.get("path").and_then(Value::as_str),
            Some("/private/uploads/asset-1/notes.txt")
        );
        assert_eq!(read.value()["futureField"]["retained"], Value::Bool(true));

        let invalid = TranscriptAssetRecord::from_value(json!({
            "id":"bad-asset", "agent":"agent-1", "name":"notes.txt",
            "mime":"text/plain", "image":false, "size":5,
            "hash":"sha256:abc", "created":1.5
        }))
        .expect("invalid schema fixture has an id");
        assert!(matches!(
            table.put(&invalid).expect_err("path is required for persisted asset"),
            StoreError::InvalidStoredObject { table: "runtime_assets", id }
                if id == "bad-asset"
        ));
    }

    #[test]
    fn insert_new_only_ignores_the_declared_id_conflict() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL CHECK(id != 'blocked'));",
            )
            .expect("checked table creates");
        connection
            .execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        let table = Table::<AgentRecord>::new(&connection);
        let blocked = agent(
            json!({"id":"blocked","provider":"codex","rootId":"blocked","epoch":0,"status":"idle"}),
        );
        assert!(matches!(
            table.insert_new(&blocked),
            Err(StoreError::Sqlite(_))
        ));
    }

    #[test]
    fn record_queries_use_only_declared_filters_and_fixed_orderings() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,created REAL,record TEXT NOT NULL);
                 INSERT INTO runtime_items VALUES('i1','a',2,'{\"id\":\"i1\",\"agent\":\"a\",\"text\":\"one\"}');
                 INSERT INTO runtime_items VALUES('i2','b',4,'{\"id\":\"i2\",\"agent\":\"b\",\"text\":\"two\"}');
                 INSERT INTO runtime_items VALUES('i3','a',3,'{\"id\":\"i3\",\"agent\":\"a\",\"text\":\"three\"}');",
            )
            .expect("transcript table creates");
        let table = Table::<TranscriptItemRecord>::new(&connection);
        let records = table
            .list_matching(
                Some(RecordPredicate::PhysicalAgentEquals("a")),
                RecordOrder::PhysicalCreatedDescIdDesc,
            )
            .expect("physical owner query");
        assert_eq!(
            records
                .iter()
                .map(|record| record.id().as_ref())
                .collect::<Vec<_>>(),
            ["i3", "i1"]
        );
        assert!(
            table
                .get_matching(
                    &TranscriptItemId::from("i1"),
                    Some(RecordPredicate::PhysicalAgentEquals("b"))
                )
                .expect("id and owner query")
                .is_none()
        );

        connection
            .execute_batch(
                "CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 INSERT INTO runtime_tasks VALUES('t1','{\"id\":\"t1\",\"agent\":\"a\",\"kind\":\"command\",\"status\":\"completed\",\"created\":1,\"finished\":3}');
                 INSERT INTO runtime_tasks VALUES('t2','{\"id\":\"t2\",\"agent\":\"a\",\"kind\":\"command\",\"status\":\"completed\",\"created\":2}');
                 INSERT INTO runtime_tasks VALUES('t3','{\"id\":\"t3\",\"agent\":\"b\",\"kind\":\"command\",\"status\":\"completed\",\"created\":0}');",
            )
            .expect("runtime task table creates");
        let agents = vec!["a".to_owned()];
        let tasks = Table::<TaskRecord>::new(&connection)
            .list_matching(
                Some(RecordPredicate::JsonAgentIn(&agents)),
                RecordOrder::JsonEffectiveCreatedAscIdAsc,
            )
            .expect("JSON field filter");
        assert_eq!(
            tasks
                .iter()
                .map(|record| record.id().as_ref())
                .collect::<Vec<_>>(),
            ["t2", "t1"]
        );
        assert!(matches!(
            Table::<AgentRecord>::new(&connection).list_matching(
                Some(RecordPredicate::JsonAgentIn(&agents)),
                RecordOrder::RowId
            ),
            Err(StoreError::RecordContract(_))
        ));
    }
}
