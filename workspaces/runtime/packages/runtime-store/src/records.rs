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

fn present_start_wait<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<studio_runtime_schema::agent_progress::AgentStartWait>, D::Error> {
    studio_runtime_schema::agent_progress::AgentStartWait::deserialize(d).map(Some)
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
pub use studio_runtime_schema::task_record::TaskId;
string_id!(QueueEventId);
string_id!(ChatMessageId);
pub use studio_runtime_schema::transcript_asset::{TranscriptAssetId, TranscriptAssetRecord};
pub use studio_runtime_schema::transcript_record::TranscriptItemId;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentExecutionMode {
    Native,
    Host,
    Vm,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentActiveTool {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentActivity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_nonnegative_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AgentActiveTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismissed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_started: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_buffering_ui: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentContextUsage {
    pub tokens: Option<u64>,
    pub window: Option<u64>,
    pub at: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentArchive {
    pub at: f64,
    #[serde(deserialize_with = "deserialize_safe_counter")]
    pub epoch: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_pending: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_tool_requests: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unassigned_work: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanedWorktree {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative: Option<String>,
    pub branch: Option<String>,
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<(Option<i64>, Option<String>, Option<f64>)>,
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSafetyBuffering {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_key: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_nonnegative_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismissed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faster_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_started: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_buffering_ui: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeStatusPhase {
    Safety,
    Retrying,
    Auth,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<NativeStatusPhase>,
    #[serde(
        default,
        deserialize_with = "deserialize_nonnegative_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentSandbox {
    ReadOnly {
        #[serde(rename = "networkAccess", skip_serializing_if = "Option::is_none")]
        network_access: Option<bool>,
    },
    DangerFullAccess,
    WorkspaceWrite {
        #[serde(rename = "writableRoots", skip_serializing_if = "Option::is_none")]
        writable_roots: Option<Vec<String>>,
        #[serde(rename = "networkAccess", skip_serializing_if = "Option::is_none")]
        network_access: Option<bool>,
        #[serde(
            rename = "excludeTmpdirEnvVar",
            skip_serializing_if = "Option::is_none"
        )]
        exclude_tmpdir_env_var: Option<bool>,
        #[serde(rename = "excludeSlashTmp", skip_serializing_if = "Option::is_none")]
        exclude_slash_tmp: Option<bool>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub id: AgentId,
    pub root_id: AgentId,
    #[serde(deserialize_with = "deserialize_agent_epoch")]
    pub epoch: i64,
    pub status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_archive: Option<AgentArchive>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<AgentSandbox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleaned_worktree: Option<CleanedWorktree>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_safety_buffering: Option<NativeSafetyBuffering>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_status: Option<NativeStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_lead: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_mode: Option<studio_runtime_schema::records::WorkspaceMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_safe_counter")]
    pub compactions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_response_turn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parked_event: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_start_wait"
    )]
    pub start_wait: Option<studio_runtime_schema::agent_progress::AgentStartWait>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_safe_counter",
        skip_serializing_if = "Option::is_none"
    )]
    pub events: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tools: Option<Vec<AgentActiveTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_answer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<AgentContextUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage_accounting: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_wake: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_resume_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<AgentExecutionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_epoch: Option<i64>,
    pub account_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_job: Option<StoredTitleJob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_attempt: Option<Box<StartAttemptView>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disconnect_recovery: Option<Box<studio_runtime_schema::recovery_record::RecoveryFields>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_recovery: Option<Box<studio_runtime_schema::recovery_record::RecoveryFields>>,
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

fn deserialize_agent_epoch<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<i64, D::Error> {
    let epoch = i64::deserialize(deserializer)?;
    if (0..=studio_runtime_schema::shared_values::MAX_SAFE_INTEGER).contains(&epoch) {
        Ok(epoch)
    } else {
        Err(serde::de::Error::custom("agent epoch outside safe range"))
    }
}

fn deserialize_safe_counter<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value <= studio_runtime_schema::shared_values::MAX_SAFE_INTEGER as u64 {
        Ok(value)
    } else {
        Err(serde::de::Error::custom("counter outside safe range"))
    }
}

pub(crate) fn deserialize_optional_safe_counter<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(transparent)]
    struct Safe(#[serde(deserialize_with = "deserialize_safe_counter")] u64);
    Ok(Option::<Safe>::deserialize(deserializer)?.map(|value| value.0))
}

fn deserialize_nonnegative_time<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    let value = Option::<f64>::deserialize(deserializer)?;
    if value.is_none_or(|time| time.is_finite() && time >= 0.0) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom("timestamp must be nonnegative"))
    }
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

pub use studio_runtime_schema::start_attempt::{
    InterruptOutcome, StartAttemptAction, StartAttemptView, StartInterruptView,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveExecutionView {
    pub run_id: Option<String>,
    pub attempt_id: Option<String>,
}

macro_rules! start_attempt_setters {
    ($($name:ident: $ty:ty => $field:ident = $convert:expr;)+) => {
        $(pub fn $name(&mut self, value: $ty) -> StoreResult<()> {
            let attempt = self.start_attempt.as_mut()
                .ok_or(crate::StoreError::RecordContract("missing start attempt"))?;
            attempt.$field = ($convert)(value);
            Ok(())
        })+
    };
}

impl AgentRecord {
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some_and(|at| at != 0.0)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_defaults() -> Self {
        Self {
            id: AgentId::from(""),
            root_id: AgentId::from(""),
            epoch: 0,
            status: AgentStatus::Unknown,
            parent_id: None,
            deleted_at: None,
            agent_archive: None,
            sandbox: None,
            cleaned_worktree: None,
            native_safety_buffering: None,
            native_status: None,
            is_lead: None,
            workspace_mode: None,
            model: None,
            cwd: None,
            name: None,
            compactions: None,
            worktree_ready: None,
            approval_policy: None,
            native_response_turn: None,
            parked_event: None,
            start_wait: None,
            events: None,
            last_event: None,
            activity: None,
            active_tools: None,
            tail: None,
            last_answer: None,
            context_usage: None,
            tokens_used: None,
            token_usage_accounting: None,
            in_flight: None,
            auto_wake: None,
            usage_resume_enabled: None,
            execution_mode: None,
            turn_epoch: None,
            account_key: crate::cost_aggregate::DEFAULT_ACCOUNT.to_owned(),
            thread_id: None,
            turn_id: None,
            title_job: None,
            start_attempt: None,
            disconnect_recovery: None,
            restart_recovery: None,
            start_outcome_hold: None,
            fired_events: BTreeSet::new(),
            native_review_thread: None,
            vm_launch: None,
            credential_copy: None,
            vm_auth_bootstrap: None,
            native_release: None,
            extra: BTreeMap::new(),
        }
    }

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

    start_attempt_setters! {
        set_start_attempt_submitted: bool => submitted = Some;
        set_start_attempt_account_key: &str => account_key = |v: &str| Some(v.to_owned());
        set_start_attempt_thread_id: Option<&str> => thread_id = |v: Option<&str>| v.map(str::to_owned);
        set_start_attempt_connection_id: Option<&str> => connection_id = |v: Option<&str>| v.map(str::to_owned);
        set_start_attempt_native_operation_id: &str => native_operation_id = |v: &str| Some(v.to_owned());
        set_start_attempt_not_submitted_reason: &str => not_submitted_reason = |v: &str| Some(v.to_owned());
        set_start_attempt_retired_events: &[String] => retired_events = |v: &[String]| Some(v.to_vec());
        set_start_attempt_turn_id: Option<&str> => turn_id = |v: Option<&str>| v.map(str::to_owned);
        set_start_attempt_observed_turn_id: Option<&str> => observed_turn_id = |v: Option<&str>| v.map(str::to_owned);
        set_start_attempt_execution_outcome: studio_runtime_schema::persistence::ExecutionStatus => execution_outcome = Some;
        set_start_attempt_request_ticket: &str => request_ticket = |v: &str| Some(v.to_owned());
        set_start_attempt_connection_generation: u64 => connection_generation = Some;
    }

    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }

    pub fn id(&self) -> &AgentId {
        &self.id
    }

    pub fn value(&self) -> Value {
        self.clone().into_value()
    }

    /// Consume the typed record while moving its fields into a JSON object.
    pub fn into_value(mut self) -> Value {
        let extra = std::mem::take(&mut self.extra);
        let mut value = crate::projection_value(&self);
        value
            .as_object_mut()
            .expect("agent is an object")
            .extend(extra);
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

    fn decode_stored(connection: &Connection, row_id: &str, encoded: &str) -> StoreResult<Self> {
        let mut agent = parse_agent_record(row_id, encoded)?;
        crate::agent_volatile::merge_agent(connection, row_id, &mut agent)?;
        Ok(agent)
    }
}

pub use studio_runtime_schema::records::WorkStatus;

pub use studio_runtime_schema::task_record::TaskRecord;
impl JsonRecord for TaskRecord {
    type Id = TaskId;
    const TABLE: &'static str = "runtime_tasks";
    const SCHEMA: &'static str = "PersistedTaskRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::JsonAgent;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
pub use studio_runtime_schema::plan_record::PlanRecord;

#[cfg(test)]
mod duplicate_stored_record_tests {
    use super::*;

    fn check<R: JsonRecord>(raw: &str, row_id: &str) {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(&format!(
            "CREATE TABLE {}(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
            R::TABLE
        ))
        .unwrap();
        db.execute(
            &format!("INSERT INTO {}(id,record) VALUES(?1,?2)", R::TABLE),
            [row_id, raw],
        )
        .unwrap();
        assert!(
            Table::<R>::new(&db)
                .get(&R::Id::from(row_id.to_owned()))
                .is_err()
        );
    }

    #[test]
    fn duplicate_stored_record_plan() {
        check::<PlanRecord>(
            r#"{"id":"first","id":"last","rootId":"r","text":"x","version":1.0,"updated":null,"extra":{"x":1}}"#,
            "external",
        );
    }
    #[test]
    fn duplicate_stored_record_annotation() {
        check::<AnnotationRecord>(r#"{"id":"other","id":"a","line":1.0,"future":null}"#, "a");
    }
    #[test]
    fn duplicate_stored_record_complaint() {
        check::<ComplaintRecord>(
            r#"{"id":"other","id":"c","status":"future","extra":[1]}"#,
            "c",
        );
    }
    #[test]
    fn duplicate_stored_record_checkpoint() {
        check::<CheckpointRecord>(r#"{"id":"other","id":"c","agent":null,"future":[1]}"#, "c");
    }
    #[test]
    fn duplicate_stored_record_monitor() {
        assert!(
            serde_json::from_str::<MonitorRecord>(r#"{"id":"other","id":"m","future":null}"#)
                .is_err()
        );
    }
    #[test]
    fn duplicate_stored_record_asset() {
        check::<TranscriptAssetRecord>(
            r#"{"id":"other","id":"a","agent":"owner","name":"n","path":"p","mime":"m","image":false,"size":1.0,"hash":"h","created":2,"future":[1]}"#,
            "a",
        );
    }
    #[test]
    fn duplicate_stored_record_workspace() {
        check::<WorkspaceOperationRecord>(
            r#"{"id":"other","id":"op","agent":"a","kind":"branch","phase":"provider_pending","other":{"opaque":[1,null]}}"#,
            "op",
        );
    }
}

impl JsonRecord for PlanRecord {
    type Id = AgentId;
    const TABLE: &'static str = "runtime_plans";
    const SCHEMA: &'static str = "PersistedPlanRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::External;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        None
    }
}
#[cfg(test)]
mod plan_typed_tests {
    use super::{AgentId, PlanRecord, Table};
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn plan_deserialization_enforces_shape_and_preserves_extensions() {
        for invalid in [
            json!({"id":"a"}),
            json!({"id":"a","rootId":"r","text":"","version":1.5,"updated":null}),
            json!({"id":"a","rootId":"r","text":"","version":1,"updated":null,"native":[]}),
        ] {
            assert!(PlanRecord::deserialize(invalid).is_err());
        }
        let value = json!({"id":"different-from-row","rootId":"r","text":"x","version":1.0,"updated":null,"native":null,"future":[1]});
        let record = PlanRecord::deserialize(value.clone()).unwrap();
        assert_eq!(record.into_value(), value);
    }

    #[test]
    fn plan_store_uses_external_key_and_rejects_duplicate() {
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
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
#[cfg(test)]
mod annotation_typed_tests {
    use super::{AnnotationRecord, Table};
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn annotation_deserialization_enforces_shapes() {
        for value in [
            json!(["a"]),
            json!({"id":"a","line":1.5}),
            json!({"id":"a","text":1}),
        ] {
            assert!(AnnotationRecord::deserialize(value).is_err());
        }
        for value in [
            json!({"id":"a"}),
            json!({"id":"a","line":1.0,"created":null,"extra":[1]}),
        ] {
            let record = AnnotationRecord::deserialize(value.clone()).unwrap();
            assert_eq!(record.into_value(), value);
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
    fn annotation_store_preserves_identity_extensions_and_rejects_duplicate() {
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
pub use studio_runtime_schema::tool_result::ToolResultRecord;
impl JsonRecord for ToolResultRecord {
    type Id = String;
    const TABLE: &'static str = "request_results";
    const SCHEMA: &'static str = "PersistedToolResult";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::External;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        None
    }
}
#[cfg(test)]
mod tool_result_typed_tests {
    use super::{Table, ToolResultRecord};
    use serde_json::json;
    #[test]
    fn tool_result_table_checks_shell_and_preserves_extensions() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE request_results(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        let table = Table::<ToolResultRecord>::new(&db);
        let input = json!({"success":null,"contentItems":[{"type":"image","text":null,"data":[1,2]}],"extension":true});
        table
            .put_at(
                &"r".into(),
                &ToolResultRecord::from_value(input.clone()).unwrap(),
            )
            .unwrap();
        assert_eq!(table.get(&"r".into()).unwrap().unwrap().into_value(), input);
        db.execute(
            "UPDATE request_results SET record=?1",
            [r#"{"success":false,"success":true}"#],
        )
        .unwrap();
        assert!(table.get(&"r".into()).is_err());
        db.execute(
            "UPDATE request_results SET record=?1",
            [r#"{"contentItems":[null]}"#],
        )
        .unwrap();
        assert!(table.get(&"r".into()).is_err());
    }
}
pub use studio_runtime_schema::request_record::RequestRecord;
impl JsonRecord for RequestRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_requests";
    const SCHEMA: &'static str = "PersistedRequestRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::JsonAgent;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&String> {
        Some(self.id())
    }
    fn encode_stored(&self, _connection: &Connection, row_id: &str) -> StoreResult<String> {
        if self.id != row_id
            || !self.created_at.is_finite()
            || !self.params.get().trim_start().starts_with('{')
        {
            return Err(crate::StoreError::InvalidStoredObject {
                table: Self::TABLE,
                id: row_id.to_owned(),
            });
        }
        Ok(serde_json::to_string(self)?)
    }
}
#[cfg(test)]
mod request_typed_tests {
    use super::{RequestRecord, Table};
    use serde_json::Value;

    #[test]
    fn shared_decoder_requires_object_for_direct_records() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Record {
            id: String,
        }
        impl super::JsonRecord for Record {
            type Id = String;
            const TABLE: &'static str = "fixture";
            const SCHEMA: &'static str = "fixture";
            const KEY_MODE: super::RecordKeyMode = super::RecordKeyMode::PayloadId;
            const QUERY_SHAPE: super::RecordQueryShape = super::RecordQueryShape::None;
            const TYPED_SHAPE: bool = true;
            fn record_key(&self) -> Option<&String> {
                Some(&self.id)
            }
        }
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        assert!(super::decode_record::<Record>(&connection, "row", r#"{"id":"row"}"#).is_ok());
        assert!(super::decode_record::<Record>(&connection, "row", r#"["row"]"#).is_err());
    }

    #[test]
    fn request_table_rejects_duplicate_and_checks_row_identity() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        db.execute("INSERT INTO runtime_requests VALUES('r',?1)", [r#"{"id":"wrong","id":"r","status":false,"status":"future","method":"x","params":{"opaque":[1,null]}}"#]).unwrap();
        assert!(Table::<RequestRecord>::new(&db).get(&"r".into()).is_err());
        db.execute(
            "UPDATE runtime_requests SET record=?1",
            [r#"{"id":"r","agent":"a","createdAt":1,"status":"pending","method":"agent/asyncQuestion","params":{"opaque":[1,null]}}"#],
        )
        .unwrap();
        let row = Table::<RequestRecord>::new(&db)
            .get(&"r".into())
            .unwrap()
            .unwrap();
        assert_eq!(row.status.as_str(), "pending");
        assert_eq!(row.params.get(), r#"{"opaque":[1,null]}"#);
        db.execute(
            "UPDATE runtime_requests SET record=?1",
            [r#"{"id":"other","status":"pending"}"#],
        )
        .unwrap();
        assert!(Table::<RequestRecord>::new(&db).get(&"r".into()).is_err());
        db.execute(
            "UPDATE runtime_requests SET record=?1",
            [r#"{"id":"r","status":false}"#],
        )
        .unwrap();
        assert!(Table::<RequestRecord>::new(&db).get(&"r".into()).is_err());
        let unchanged: Value = serde_json::from_str(
            &db.query_row("SELECT record FROM runtime_requests", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(unchanged["status"], false);
    }
}
pub use studio_runtime_schema::complaint_record::ComplaintRecord;
impl JsonRecord for ComplaintRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_complaints";
    const SCHEMA: &'static str = "PersistedComplaintRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
#[cfg(test)]
mod complaint_typed_tests {
    use super::{ComplaintRecord, Table};
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn complaint_deserialization_enforces_closed_shape() {
        for value in [
            json!(["c"]),
            json!({"id":"c","author":1}),
            json!({"id":"c","status":false}),
            json!({"id":"c","leadId":[]}),
        ] {
            assert!(ComplaintRecord::deserialize(value).is_err());
        }
        let value = json!({"id":"c","author":null,"leadId":"l","text":null,"status":"open","recipient":"lead","version":1,"created":1.0,"updated":1.0,"readAt":null,"responses":[]});
        let decoded = ComplaintRecord::deserialize(value.clone()).unwrap();
        assert_eq!(decoded.into_value(), value);
    }

    #[test]
    fn complaint_storage_preserves_bytes_and_enforces_identity() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        let raw = r#"{ "id":"c", "status":"open", "author":null, "leadId":"l", "text":null, "recipient":"lead", "version":1, "created":1.0, "updated":1.0, "readAt":null, "responses":[] }"#;
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
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
pub use studio_runtime_schema::monitor_record::MonitorRecord;
impl JsonRecord for MonitorRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_monitors";
    const SCHEMA: &'static str = "PersistedMonitorRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::JsonAgent;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
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
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn terminal_deserialization_enforces_schema_and_preserves_fields() {
        for invalid in [
            json!(["t"]),
            json!({"id":"t","agent":7}),
            json!({"id":"t","status":false}),
            json!({"id":"t","exitCode":1.5}),
        ] {
            assert!(TerminalRecord::deserialize(invalid).is_err());
        }
        for valid in [
            json!({"id":"t"}),
            json!({"id":"t","agent":null,"status":null,"exitCode":null,"extra":{"a":1}}),
            json!({"id":"t","exitCode":1.0}),
            json!({"id":"t","exitCode":u64::MAX}),
        ] {
            let record = TerminalRecord::deserialize(valid.clone()).unwrap();
            assert_eq!(record.value(), valid);
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
        let mut conflicting = record;
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
        assert!(
            Table::<TerminalRecord>::new(&tx)
                .get(&"t".to_string())
                .is_err()
        );
    }
}
pub use studio_runtime_schema::usage_resume_record::UsageResumeRecord;
impl JsonRecord for UsageResumeRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_usage_resumes";
    const SCHEMA: &'static str = "PersistedUsageResumeRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::External;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::UsageResumeRecovery;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&String> {
        None
    }
}
/// Historical public receipts validated these five fields and retained all other JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyWorkReply {
    value: Value,
    fields: WorkItemFields,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct WorkItemFields {
    id: TaskId,
    status: WorkStatus,
    #[serde(default)]
    dependencies: Vec<String>,
    owner: Option<String>,
    #[serde(default)]
    updated: f64,
}
impl Serialize for LegacyWorkReply {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for LegacyWorkReply {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl LegacyWorkReply {
    pub fn id(&self) -> &TaskId {
        &self.fields.id
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.value.get(key)
    }
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let fields = WorkItemFields::deserialize(&value)?;
        Ok(Self { value, fields })
    }
    pub fn value(&self) -> &Value {
        &self.value
    }
    pub fn into_value(self) -> Value {
        self.value
    }
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), serde_json::Error> {
        let mut updated = self.value.clone();
        updated[key] = value;
        let updated = Self::from_value(updated)?;
        if updated.fields.id != self.fields.id {
            return Err(serde::de::Error::custom("work record id is immutable"));
        }
        *self = updated;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkItemRecord(studio_runtime_schema::work_item_record::WorkItem);
impl JsonRecord for WorkItemRecord {
    type Id = TaskId;
    const TABLE: &'static str = "runtime_work";
    const SCHEMA: &'static str = "PersistedWorkItemRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
    fn decode_stored(_connection: &Connection, row_id: &str, encoded: &str) -> StoreResult<Self> {
        Self::decode(row_id, encoded)
    }
}
/// The stored run uses the execution-history owner with unconstrained historical evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ExecutionRunRecord(crate::ExecutionRun<Value, studio_runtime_schema::AgentCounter>);
impl<'de> Deserialize<'de> for ExecutionRunRecord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let studio_runtime_schema::json_object::JsonObject(run) =
            studio_runtime_schema::json_object::JsonObject::deserialize(deserializer)?;
        Ok(Self(run))
    }
}
impl ExecutionRunRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
    pub fn run(&self) -> &crate::ExecutionRun<Value, studio_runtime_schema::AgentCounter> {
        &self.0
    }
}
impl JsonRecord for ExecutionRunRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_execution_runs";
    const SCHEMA: &'static str = "PersistedExecutionRunRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(&self.0.id)
    }
}
#[cfg(test)]
mod execution_run_record_tests {
    use super::{ExecutionRunRecord, Table};
    use serde::Serialize;
    use serde_json::{Value, json};

    #[test]
    fn execution_run_table_preserves_evidence_and_checks_physical_identity() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .unwrap();
        let raw = r#"{"id":"run","agent":"a","accountKey":"default","epoch":1.0,"created":1.0,"status":"running","latestAttemptId":null,"rootAttemptId":{"opaque":[1]},"extra":9007199254740993}"#;
        db.execute("INSERT INTO runtime_execution_runs VALUES('run',?1)", [raw])
            .unwrap();
        let record = Table::<ExecutionRunRecord>::new(&db)
            .get(&"run".into())
            .unwrap()
            .unwrap();
        let value = record.serialize(serde_json::value::Serializer).unwrap();
        let expected: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(value, expected);
        assert_eq!(value["extra"], json!(9007199254740993_u64));
        assert!(value.get("threadId").is_none());
        db.execute(
            "UPDATE runtime_execution_runs SET record=?1",
            [raw.replace("\"id\":\"run\"", "\"id\":\"other\"")],
        )
        .unwrap();
        assert!(
            Table::<ExecutionRunRecord>::new(&db)
                .get(&"run".into())
                .is_err()
        );
    }
}
pub use studio_runtime_schema::workspace_operation::WorkspaceOperationRecord;
impl JsonRecord for WorkspaceOperationRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_workspace_operations";
    const SCHEMA: &'static str = "PersistedWorkspaceOperationRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(&self.id)
    }
}
pub use studio_runtime_schema::rule_record::RuleRecord;
impl JsonRecord for RuleRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_rules";
    const SCHEMA: &'static str = "PersistedRuleRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
pub use studio_runtime_schema::transcript_record::TranscriptItemRecord;
impl JsonRecord for TranscriptItemRecord {
    type Id = TranscriptItemId;
    const TABLE: &'static str = "runtime_items";
    const SCHEMA: &'static str = "PersistedTranscriptRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::PhysicalAgent;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
    fn encode_stored(&self, connection: &Connection, row_id: &str) -> StoreResult<String> {
        if self.id().as_ref() != row_id {
            return Err(crate::StoreError::InvalidStoredObject {
                table: Self::TABLE,
                id: row_id.to_owned(),
            });
        }
        if !self.has_complete_inputs() {
            return Err(crate::StoreError::TranscriptInputsIncomplete {
                id: row_id.to_owned(),
            });
        }
        if matches!(
            self.text(),
            studio_runtime_schema::transcript_record::TranscriptText::Inline(_)
        ) {
            return Ok(serde_json::to_string(self)?);
        }
        let text = connection
            .query_row(
                "SELECT body FROM runtime_item_fulltext WHERE id=?1",
                [row_id],
                |row| row.get::<_, Option<crate::stored_text::StoredText>>(0),
            )
            .optional()?
            .flatten()
            .map(crate::stored_text::StoredText::into_string)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| crate::StoreError::TranscriptFullTextUnavailable {
                id: row_id.to_owned(),
            })?;
        let mut hydrated = self.clone();
        hydrated.set_inline_text(text);
        Ok(serde_json::to_string(&hydrated)?)
    }
}
impl JsonRecord for TranscriptAssetRecord {
    type Id = TranscriptAssetId;
    const TABLE: &'static str = "runtime_assets";
    const SCHEMA: &'static str = "PersistedTranscriptAsset";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::JsonAgent;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        Some(self.id())
    }
}
pub use studio_runtime_schema::event_metadata::EventMetadataRecord;
impl JsonRecord for EventMetadataRecord {
    type Id = QueueEventId;
    const TABLE: &'static str = "runtime_event_meta";
    const SCHEMA: &'static str = "RuntimeEventMetadataRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::External;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    const TYPED_SHAPE: bool = true;
    fn record_key(&self) -> Option<&Self::Id> {
        None
    }
}

impl WorkItemRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
    pub fn checked(value: &Value) -> StoreResult<Self> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_owned();
        Self::checked_at(value, &id)
    }
    fn checked_at(value: &Value, row_id: &str) -> StoreResult<Self> {
        #[cfg(test)]
        validation_counts::CHECKS.with(|count| count.set(count.get() + 1));
        match Self::deserialize(value) {
            Ok(record) => Ok(record),
            Err(error) => {
                if let Some((pointer, reason)) =
                    studio_runtime_schema::first_error(Self::SCHEMA, value)
                        .map_err(crate::StoreError::SchemaValidation)?
                {
                    return Err(crate::StoreError::InvalidSyncObjectField {
                        scope: Self::TABLE.into(),
                        id: row_id.into(),
                        pointer,
                        reason,
                    });
                }
                Err(crate::StoreError::InvalidSyncObjectField {
                    scope: Self::TABLE.into(),
                    id: row_id.into(),
                    pointer: String::new(),
                    reason: error.to_string(),
                })
            }
        }
    }
    pub fn decode(row_id: &str, encoded: &str) -> StoreResult<Self> {
        let invalid = || crate::StoreError::InvalidStoredObject {
            table: Self::TABLE,
            id: row_id.into(),
        };
        let value = serde_json::from_str(encoded).map_err(|_| invalid())?;
        let record = Self::checked_at(&value, row_id)?;
        if record.id().as_ref() != row_id {
            return Err(invalid());
        }
        Ok(record)
    }
    pub fn id(&self) -> &TaskId {
        self.0.id()
    }
    pub fn value(&self) -> Value {
        self.0.value()
    }
    pub fn into_value(self) -> Value {
        self.0.into_value()
    }
    pub fn extension(&self, key: &str) -> Option<&Value> {
        self.0.extra(key)
    }
    pub fn status(&self) -> WorkStatus {
        self.0.status()
    }
    pub fn dependencies(&self) -> &[String] {
        self.0.dependencies()
    }
    pub fn owner(&self) -> Option<&str> {
        self.0.owner()
    }
    pub fn root_id(&self) -> Option<&str> {
        self.0.root_id()
    }
    pub fn title(&self) -> Option<&str> {
        self.0.title()
    }
    pub fn updated(&self) -> f64 {
        self.0.updated()
    }
    pub fn version(&self) -> Option<&serde_json::Number> {
        self.0.version()
    }
    pub fn results(&self) -> Option<&[serde_json::Map<String, Value>]> {
        self.0.results()
    }
    pub fn decisions(&self) -> Option<&[serde_json::Map<String, Value>]> {
        self.0.decisions()
    }
    pub fn results_mut(&mut self) -> Option<&mut Vec<serde_json::Map<String, Value>>> {
        self.0.results_mut()
    }
    pub fn decisions_mut(&mut self) -> Option<&mut Vec<serde_json::Map<String, Value>>> {
        self.0.decisions_mut()
    }
    pub fn blocked_by(&self, statuses: &BTreeMap<String, WorkStatus>) -> Vec<String> {
        self.dependencies()
            .iter()
            .filter(|id| statuses.get(*id) != Some(&WorkStatus::Accepted))
            .cloned()
            .collect()
    }
    pub fn set(&mut self, key: impl Into<String>, value: Value) -> Result<(), serde_json::Error> {
        self.0.set(key, value)
    }
    pub fn set_status(&mut self, status: WorkStatus) {
        self.0.set_status(status);
    }
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

    fn record_key(&self) -> Option<&Self::Id>;

    fn encode_stored(&self, _connection: &Connection, row_id: &str) -> StoreResult<String>
    where
        Self: Sized,
    {
        Ok(serde_json::to_string(&checked_value(self, row_id)?)?)
    }

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
        if R::KEY_MODE != RecordKeyMode::PayloadId
            || R::TABLE == AgentRecord::TABLE
            || R::TABLE == MonitorRecord::TABLE
        {
            return Err(crate::StoreError::RecordContract(
                "external-key records must be written with put_at",
            ));
        }
        let key = record
            .record_key()
            .ok_or(crate::StoreError::RecordContract(
                "payload-id record has no cached id",
            ))?;
        let encoded = encode_record(self.connection, record, key.as_ref())?;
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
        let encoded = encode_record(self.connection, record, id)?;
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
        let encoded = encode_record(self.connection, record, id)?;
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
    _connection: &Connection,
    row_id: &str,
    encoded: &str,
) -> StoreResult<R> {
    let invalid = || crate::StoreError::InvalidStoredObject {
        table: R::TABLE,
        id: row_id.to_owned(),
    };
    let record = if R::TYPED_SHAPE {
        serde_json::from_str::<studio_runtime_schema::json_object::JsonObject<R>>(encoded)
            .map_err(|_| invalid())?
            .0
    } else {
        let value = decode_value(R::SCHEMA, encoded, invalid)?;
        studio_runtime_schema::json_object::JsonObject::<R>::deserialize(value)
            .map_err(|_| invalid())?
            .0
    };
    if R::KEY_MODE == RecordKeyMode::PayloadId
        && record.record_key().map(AsRef::as_ref) != Some(row_id)
    {
        return Err(invalid());
    }
    Ok(record)
}

/// Decode a complete agent from its stored row and volatile columns.
pub fn decode_agent_record(
    connection: &Connection,
    row_id: &str,
    encoded: &str,
) -> StoreResult<AgentRecord> {
    AgentRecord::decode_stored(connection, row_id, encoded)
}

pub(crate) fn parse_agent_record(row_id: &str, encoded: &str) -> StoreResult<AgentRecord> {
    #[cfg(any(test, feature = "test-support"))]
    crate::agent_volatile::publication_work::record(
        row_id,
        crate::agent_volatile::publication_work::Operation::Decode,
    );
    let agent: AgentRecord = serde_json::from_str(encoded)
        .map_err(|_| invalid_agent_field(row_id, "", "record_decode", None))?;
    if agent.id.0 != row_id {
        return Err(invalid_agent_field(row_id, "/id", "row_id_mismatch", None));
    }
    Ok(agent)
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

fn encode_record<R: JsonRecord>(
    connection: &Connection,
    record: &R,
    row_id: &str,
) -> StoreResult<String> {
    #[cfg(test)]
    validation_counts::ENCODINGS.with(|count| count.set(count.get() + 1));
    record.encode_stored(connection, row_id)
}

/// The record as JSON, checked against its schema and row key.
fn checked_value<R: JsonRecord>(record: &R, row_id: &str) -> StoreResult<Value> {
    #[cfg(test)]
    validation_counts::CHECKS.with(|count| count.set(count.get() + 1));
    let value = record.serialize(serde_json::value::Serializer)?;
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
    #[test]
    fn agent_record_requires_materialized_account() {
        let old = r#"{"id":"agent","rootId":"agent","epoch":1,"status":"idle"}"#;
        assert!(serde_json::from_str::<super::AgentRecord>(old).is_err());
    }
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
        let legacy = r#"{"id":"agent","rootId":"agent","epoch":1,"status":"idle","accountKey":"default","compactions":0,"nativeReviewThread":{"attempt":"attempt","connection":"connection","generation":2,"operation":"operation","requestDigest":"digest","sequence":1,"status":"accepted","threadId":"thread"},"nativeRelease":{"id":"release","phase":"released","resetPending":false}}"#;
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

        let current = r#"{"id":"agent","rootId":"agent","epoch":1,"status":"idle","accountKey":"default","compactions":0,"vmLaunch":{"identity":{"agentId":"agent","epoch":1,"handle":"handle","requestId":"request","launchFingerprint":"digest"},"signature":"signature","status":"submitted"},"credentialCopy":{"agentId":"agent","epoch":1,"accountKey":"account","provider":"codex","relativePath":"path","digest":"digest","requestId":"request","signature":"signature","status":"submitted"},"vmAuthBootstrap":{"agentId":"agent","epoch":1,"requestId":"request","rootRequestId":"root","admittedRequest":{"id":"token","signature":"signature","created":1},"fingerprint":"digest","state":"submitted"},"nativeRelease":{"scope":{"account":"account","connection":"connection","generation":2},"admittedRequest":{"id":"token","signature":"signature","created":1},"id":"release"}}"#;
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

    fn agent(mut value: Value) -> AgentRecord {
        value
            .as_object_mut()
            .expect("agent fixture")
            .entry("compactions")
            .or_insert(json!(0));
        AgentRecord::from_value(value).expect("agent record has a string id")
    }

    #[test]
    fn agent_rejects_malformed_start_attempt_on_decode() {
        let raw = r#"{"id":"agent","rootId":"agent","epoch":1,"status":"idle","accountKey":"default","compactions":0,"startAttempt":{"id":"attempt","submitted":"false"}}"#;
        assert!(serde_json::from_str::<AgentRecord>(raw).is_err());
    }

    #[test]
    fn start_attempt_view_and_field_writes_preserve_existing_payloads() {
        // Shapes from the execution migration and the scheduler's claim writer.
        for attempt in [
            json!({"id":"attempt-0"}),
            json!({"id":"attempt","epoch":1,"events":["event"],"submitted":false,
                "activeAtReservation":false,"accountKey":"default","created":1.0,
                "nativeRequest":{"ticket":{"operation":"op","agent":"agent","epoch":1,"account":"default","connection":"c","generation":1,"request":"r"},"method":"m","params":{"vendorExtension":[1,2]}}}),
        ] {
            let mut record = agent(
                json!({"compactions":0,"accountKey":"default","id":"agent","rootId":"agent","epoch":1,"status":"idle","startAttempt":attempt}),
            );
            let before = record.start_attempt.clone().expect("attempt");
            let view = record.start_attempt.as_ref().expect("attempt");
            assert!(view.id.is_some());
            assert_eq!(record.start_attempt.as_ref(), Some(&before));
            record
                .set_start_attempt_submitted(false)
                .expect("field write");
            let mut expected = before;
            expected.submitted = Some(false);
            assert_eq!(record.start_attempt.as_ref(), Some(&expected));
        }
    }

    #[test]
    fn start_attempt_action_view_rejects_unnormalized_values() {
        for action in ["compact", "review", "capacity", "safety"] {
            let record = agent(
                json!({"compactions":0,"accountKey":"default","id":"agent","rootId":"agent","epoch":1,"status":"idle","startAttempt":{"action":action}}),
            );
            assert!(
                record
                    .start_attempt
                    .as_ref()
                    .expect("attempt")
                    .action
                    .is_some()
            );
        }
    }

    #[test]
    fn active_execution_view_and_field_writes_preserve_migration_record() {
        let mut record = agent(
            json!({"compactions":0,"accountKey":"default","id":"agent","rootId":"agent","epoch":1,"status":"idle",
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
        let record = agent(json!({"compactions":0,"accountKey":"default",
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
            json!({"compactions":0,
                "id":"agent-1", "provider":"codex", "rootId":"root-1", "epoch":3, "status":"running", "accountKey":"default",
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
    fn transcript_record_preserves_opaque_extension_bytes() {
        let raw = r#"{"id":"item-1","role":"user","text":"body","nested":[ { "retained":true } ],"inputs":[{"text":"input","blob":[ {"v":1} ]}]}"#;
        let record: TranscriptItemRecord = serde_json::from_str(raw).expect("record");
        let encoded = serde_json::to_string(&record).expect("serialize");
        assert!(encoded.contains(r#"[ { "retained":true } ]"#));
        assert!(encoded.contains(r#"[ {"v":1} ]"#));
    }

    fn task(value: &Value) -> TaskRecord {
        serde_json::from_str(&value.to_string()).expect("runtime task has a string id")
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
    fn stored_agent_decodes_typed_execution_fields_and_preserves_extensions() {
        let connection = database();
        let value = json!({"compactions":0,
            "id":"agent", "rootId":"agent", "epoch":2, "status":"idle",
            "inFlight":true, "autoWake":false, "executionMode":"native", "turnEpoch":2,
            "accountKey":"account", "threadId":"thread", "turnId":"turn", "future":42
        });
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES (?1, ?2)",
                ["agent", &value.to_string()],
            )
            .unwrap();
        let record = Table::<AgentRecord>::new(&connection)
            .get(&AgentId::from("agent"))
            .unwrap()
            .unwrap();
        assert_eq!(record.in_flight, Some(true));
        assert_eq!(record.auto_wake, Some(false));
        assert_eq!(
            record.execution_mode,
            Some(super::AgentExecutionMode::Native)
        );
        assert_eq!(record.turn_epoch, Some(2));
        assert_eq!(record.account_key, "account");
        assert_eq!(record.thread_id.as_deref(), Some("thread"));
        assert_eq!(record.turn_id.as_deref(), Some("turn"));
        assert_eq!(record.value(), value);
    }

    #[test]
    fn agent_round_trip_keeps_unknown_json_fields_and_number_kinds() {
        let value = json!({"compactions":0,
            "id":"agent-1", "provider":"codex", "rootId":"agent-1", "epoch":2, "status":"idle", "accountKey":"default",
            "futureObject":{"number":1.5,"flag":true}, "futureInteger":9007199254740993_i64
        });
        let record = agent(value.clone());
        let rewritten = record.into_value();
        assert_eq!(rewritten["futureObject"]["number"].as_f64(), Some(1.5));
        assert_eq!(rewritten["futureObject"]["flag"].as_bool(), Some(true));
        assert_eq!(rewritten["futureInteger"].as_i64(), Some(9007199254740993));
        assert_eq!(rewritten, value);
    }

    #[test]
    fn agent_round_trip_keeps_legacy_private_fields() {
        for private in [
            json!({"accountKey":"default","titleJob":{"legacy":true}}),
            json!({"startOutcomeHold":{"stage":"held","reason":"pending"}}),
            json!({"startOutcomeHold":{"stage":"account_settlement","cursor":null,"reason":"pending"}}),
            json!({"startOutcomeHold":{"stage":"held","operationId":"op","future":1}}),
            json!({"startOutcomeHold":{"stage":"recovery","epoch":2,"reason":"pending"}}),
            json!({"titleJob":{"requestId":"r","agentId":"agent","rootRequestId":"root","admittedRequest":{"id":"r","signature":"s","created":1},"status":"pending","future":true}}),
        ] {
            let mut value = json!({"compactions":0,"id":"agent","rootId":"agent","epoch":1,"status":"idle","accountKey":"default"});
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
        let record =
            WorkItemRecord::checked(&raw).expect("nullable owner and absent default dependencies");
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
        let runtime_task = task(&json!({
            "id":"task-1", "agent":"agent-1", "kind":"command", "status":"completed",
            "created":1.25, "itemId":"item", "turnId":"turn", "type":"commandExecution", "name":"commandExecution", "interactive":true, "exitCode":7
        }));
        let rewritten = crate::projection_value(&runtime_task);
        assert_eq!(rewritten["interactive"], json!(true));
        assert_eq!(rewritten["exitCode"].as_i64(), Some(7));

        let work = work(json!({
            "id":"work-1", "rootId":"agent-1", "status":"ready", "title":"T",
            "created":1.25, "updated":1.5, "version":3, "futureField":true
        }));
        assert_eq!(work.status(), WorkStatus::Ready);
        assert_eq!(work.into_value()["futureField"], json!(true));
    }

    #[test]
    fn persisted_task_and_request_records_accept_python_written_fields() {
        let task = task(&json!({
            "id":"task-1", "agent":"agent", "created":1, "itemId":"item", "turnId":"turn", "kind":"command", "status":"running", "type":"commandExecution", "name":"commandExecution", "reattachRecovery":{"status":"running"},
            "outputPreviewOnly":true
        }));
        assert_eq!(
            crate::projection_value(&task)["reattachRecovery"]["status"],
            "running"
        );
        assert_eq!(
            crate::projection_value(&task)["outputPreviewOnly"],
            Value::Bool(true)
        );

        let request: RequestRecord = serde_json::from_str(r#"{"id":"request-1","agent":"a","method":"agent/asyncQuestion","status":"pending","createdAt":1,"params":{},"connectionId":null}"#)
        .expect("Python may persist a request without a connection identity");
        assert!(request.connection_id.is_none());
    }

    #[test]
    fn unknown_agent_status_and_fields_survive_carrier_rewrite() {
        let value = json!({"compactions":0,"id":"agent-1","provider":"codex","rootId":"agent-1","epoch":4,"accountKey":"default",
            "status":"future-agent-state","future":{"enabled":true,"count":9007199254740993_i64}});
        let record = agent(value.clone());
        assert_eq!(
            record.status,
            AgentStatus::Other(UnknownAgentStatus::new("future-agent-state".into()).unwrap())
        );
        assert_eq!(record.into_value(), value);
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
        let first = agent(
            json!({"compactions":0,"accountKey":"default","id":"a1","provider":"codex","rootId":"a1","epoch":0,"status":"idle"}),
        );
        let second = agent(
            json!({"compactions":0,"accountKey":"default","id":"a2","provider":"codex","rootId":"a2","epoch":0,"status":"queued"}),
        );

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
                "INSERT INTO runtime_agents(id,record) VALUES ('outer-id','{\"id\":\"inner-id\",\"rootId\":\"inner-id\",\"epoch\":0,\"status\":\"idle\",\"accountKey\":\"default\",\"compactions\":0}')",
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
                assert!(error.to_string().contains("record_decode"));
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
            if scope == "runtime_agents" && id == "invalid-epoch" && pointer.is_empty())
        );
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("record_decode"));
        assert!(!diagnostic.contains("private user text"));
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
                .into_value(),
            json!({"timing":{"enqueuedAt":9}})
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
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        connection
            .execute_batch("CREATE TABLE runtime_assets(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("assets table creates");

        let asset = TranscriptAssetRecord::from_value(json!({
            "id":"asset-1", "agent":"agent-1", "name":"notes.txt",
            "path":path, "mime":"text/plain",
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
        assert_eq!(read.path(), path.to_str().unwrap());
        assert_eq!(
            read.into_value()["futureField"]["retained"],
            Value::Bool(true)
        );

        let invalid = TranscriptAssetRecord::from_value(json!({
            "id":"bad-asset", "agent":"agent-1", "name":"notes.txt",
            "mime":"text/plain", "image":false, "size":5,
            "hash":"sha256:abc", "created":1.5
        }));
        assert!(invalid.is_err());
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
            json!({"compactions":0,"accountKey":"default","id":"blocked","provider":"codex","rootId":"blocked","epoch":0,"status":"idle"}),
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
                 INSERT INTO runtime_items VALUES('i1','a',2,'{\"id\":\"i1\",\"role\":\"assistant\",\"agent\":\"a\",\"text\":\"one\"}');
                 INSERT INTO runtime_items VALUES('i2','b',4,'{\"id\":\"i2\",\"role\":\"assistant\",\"agent\":\"b\",\"text\":\"two\"}');
                 INSERT INTO runtime_items VALUES('i3','a',3,'{\"id\":\"i3\",\"role\":\"assistant\",\"agent\":\"a\",\"text\":\"three\"}');",
            )
            .expect("transcript table creates");
        let table = Table::<TranscriptItemRecord>::new(&connection);
        connection
            .execute(
                "INSERT INTO runtime_items VALUES('duplicate','a',0,?1)",
                [r#"{"id":"old","id":"duplicate"}"#],
            )
            .expect("duplicate-key fixture");
        assert!(table.get(&TranscriptItemId::from("duplicate")).is_err());
        connection
            .execute("DELETE FROM runtime_items WHERE id='duplicate'", [])
            .expect("remove duplicate fixture");
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
                 INSERT INTO runtime_tasks VALUES('t1','{\"id\":\"t1\",\"itemId\":\"t1\",\"turnId\":\"turn\",\"type\":\"commandExecution\",\"name\":\"command\",\"agent\":\"a\",\"kind\":\"command\",\"status\":\"completed\",\"created\":1,\"finished\":3}');
                 INSERT INTO runtime_tasks VALUES('t2','{\"id\":\"t2\",\"itemId\":\"t2\",\"turnId\":\"turn\",\"type\":\"commandExecution\",\"name\":\"command\",\"agent\":\"a\",\"kind\":\"command\",\"status\":\"completed\",\"created\":2}');
                 INSERT INTO runtime_tasks VALUES('t3','{\"id\":\"t3\",\"itemId\":\"t3\",\"turnId\":\"turn\",\"type\":\"commandExecution\",\"name\":\"command\",\"agent\":\"b\",\"kind\":\"command\",\"status\":\"completed\",\"created\":0}');",
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

#[cfg(test)]
mod agent_projection_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stored_agent_rejects_legacy_start_wait_shapes() {
        for wait in [r#"{"reason":"slot","epoch":1,"runningTurns":null}"#, "null"] {
            let raw = format!(
                r#"{{"id":"a","rootId":"a","epoch":1,"status":"queued","accountKey":"default","compactions":0,"startWait":{wait}}}"#
            );
            assert!(super::parse_agent_record("a", &raw).is_err());
        }
    }

    #[test]
    fn stored_agent_reads_typed_volatile_columns() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":0}"#],
        )
        .unwrap();
        db.execute("INSERT INTO runtime_agent_volatile(id,events,tail,contextUsage) VALUES('a','3','\"answer\"','{\"tokens\":20,\"window\":100,\"at\":1.5}')", []).unwrap();
        let agent = Table::<AgentRecord>::new(&db)
            .get(&AgentId::from("a"))
            .unwrap()
            .unwrap();
        assert_eq!(agent.events, Some(3));
        assert_eq!(agent.tail.as_deref(), Some("answer"));
        assert_eq!(agent.context_usage.as_ref().unwrap().tokens, Some(20));
    }

    #[test]
    fn typed_agent_can_clear_volatile_activity() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":0}"#],
        )
        .unwrap();
        db.execute(
            "INSERT INTO runtime_agent_volatile(id,activity) VALUES('a',?1)",
            [r#"{"phase":"thinking","at":1.5}"#],
        )
        .unwrap();
        let mut agent = Table::<AgentRecord>::new(&db)
            .get(&AgentId::from("a"))
            .unwrap()
            .unwrap();
        agent.activity = None;
        crate::agent_volatile::put(&db, "a", &agent.value(), false).unwrap();
        assert!(
            Table::<AgentRecord>::new(&db)
                .get(&AgentId::from("a"))
                .unwrap()
                .unwrap()
                .activity
                .is_none()
        );
    }

    #[test]
    fn stored_agent_rejects_malformed_archive() {
        let raw = r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":0,"agentArchive":{"at":"bad","epoch":1}}"#;
        assert!(serde_json::from_str::<AgentRecord>(raw).is_err());
    }

    #[test]
    fn stored_agent_rejects_malformed_sandbox() {
        let raw = r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":0,"sandbox":{"type":"invalid"}}"#;
        assert!(serde_json::from_str::<AgentRecord>(raw).is_err());
    }

    #[test]
    fn stored_agent_rejects_malformed_cleanup_receipt() {
        let raw = r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":0,"cleanedWorktree":{"bytes":"bad"}}"#;
        assert!(serde_json::from_str::<AgentRecord>(raw).is_err());
    }

    #[test]
    fn stored_agent_rejects_malformed_safety_buffering() {
        let raw = r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":0,"nativeSafetyBuffering":{"showBufferingUi":"yes"}}"#;
        assert!(serde_json::from_str::<AgentRecord>(raw).is_err());
    }

    #[test]
    fn stored_agent_rejects_malformed_parent_and_deletion_marker() {
        for field in [r#""parentId":false"#, r#""deletedAt":"bad""#] {
            let raw = format!(
                r#"{{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":0,{field}}}"#
            );
            assert!(serde_json::from_str::<AgentRecord>(&raw).is_err());
        }
    }

    #[test]
    fn stored_agent_epoch_must_be_nonnegative_and_safe() {
        for epoch in [
            -1_i64,
            studio_runtime_schema::shared_values::MAX_SAFE_INTEGER + 1,
        ] {
            let raw = format!(
                r#"{{"id":"a","rootId":"a","epoch":{epoch},"status":"idle","accountKey":"default","compactions":0}}"#
            );
            assert!(serde_json::from_str::<AgentRecord>(&raw).is_err());
        }
    }

    #[test]
    fn stored_agent_rejects_unsafe_counters_and_negative_activity_time() {
        let raw = format!(
            r#"{{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":{}}}"#,
            studio_runtime_schema::shared_values::MAX_SAFE_INTEGER + 1
        );
        assert!(serde_json::from_str::<AgentRecord>(&raw).is_err());
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":0}"#],
        )
        .unwrap();
        db.execute(
            "INSERT INTO runtime_agent_volatile(id,events,activity) VALUES('a',?1,?2)",
            [
                format!(
                    "{}",
                    studio_runtime_schema::shared_values::MAX_SAFE_INTEGER + 1
                ),
                r#"{"at":1}"#.to_string(),
            ],
        )
        .unwrap();
        assert!(
            Table::<AgentRecord>::new(&db)
                .get(&AgentId::from("a"))
                .is_err()
        );
        db.execute(
            "UPDATE runtime_agent_volatile SET events='1', activity=?1 WHERE id='a'",
            [r#"{"at":-1}"#],
        )
        .unwrap();
        assert!(
            Table::<AgentRecord>::new(&db)
                .get(&AgentId::from("a"))
                .is_err()
        );
    }

    #[test]
    fn stored_agent_requires_explicit_compactions_but_accepts_unknown() {
        let missing = r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default"}"#;
        let unknown = r#"{"id":"a","rootId":"a","epoch":1,"status":"idle","accountKey":"default","compactions":null}"#;
        assert!(serde_json::from_str::<AgentRecord>(missing).is_err());
        assert_eq!(
            serde_json::from_str::<AgentRecord>(unknown)
                .unwrap()
                .compactions,
            None
        );
    }

    #[test]
    fn stored_agent_rejects_malformed_business_fields() {
        let db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .expect("agents");
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile");
        let raw = json!({"compactions":0,"id":"a","rootId":"r","epoch":1,"status":"future","accountKey":null,
            "activity":false,"titleJob":{"malformed":true},"startOutcomeHold":[null],"vmLaunch":false,
            "vendor":{"nested":[null,true]}});
        db.execute(
            "INSERT INTO runtime_agents(id,record) VALUES('a',?1)",
            [raw.to_string()],
        )
        .expect("agent");
        db.execute(
            "INSERT INTO runtime_agent_volatile(id,activity,tail) VALUES('a',?1,?2)",
            [r#"{"phase":"active","vendor":true}"#, r#""answer""#],
        )
        .expect("volatile row");
        assert!(decode_agent_record(&db, "a", &raw.to_string()).is_err());
    }

    #[test]
    fn fractional_epoch_is_rejected_by_business_decode() {
        let db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .expect("agents");
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile");
        let raw = r#"{"id":"a","rootId":"r","epoch":1.0,"status":"idle","accountKey":"default","compactions":0}"#;
        db.execute(
            "INSERT INTO runtime_agents(id,record) VALUES('a',?1)",
            [raw],
        )
        .expect("agent");
        assert!(decode_agent_record(&db, "a", raw).is_err());
        assert!(
            Table::<AgentRecord>::new(&db)
                .get(&AgentId::from("a"))
                .is_err()
        );
    }

    #[test]
    fn stored_agent_business_decode_preserves_checked_wire_fields() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        let raw = json!({"id":"a","rootId":"r","epoch":1,"status":"idle", "accountKey":"default", "compactions":0,
            "events":1,"agentArchive":{"at":1.5,"epoch":1},"future":{"x":[1,null]},
            "titleJob":{"legacy":true},"startOutcomeHold":{"stage":"held","reason":"pending"},"firedEvents":["one"]});
        db.execute(
            "INSERT INTO runtime_agents(id,record) VALUES('a',?1)",
            [raw.to_string()],
        )
        .unwrap();
        let record = Table::<AgentRecord>::new(&db)
            .get(&AgentId::from("a"))
            .unwrap()
            .unwrap();
        let value = record.into_value();
        for (key, expected) in raw.as_object().unwrap() {
            assert_eq!(&value[key], expected);
        }
    }

    #[test]
    fn stored_agent_account_key_must_be_present() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .unwrap();
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        for (account, valid) in [
            (None, false),
            (Some(json!(null)), false),
            (Some(json!(12)), false),
            (Some(json!("custom")), true),
        ] {
            let mut raw = json!({"id":"a","rootId":"a","epoch":1,"status":"idle","compactions":0});
            if let Some(value) = account {
                raw["accountKey"] = value;
            }
            db.execute(
                "INSERT OR REPLACE INTO runtime_agents VALUES ('a', ?1)",
                [raw.to_string()],
            )
            .unwrap();
            let result = Table::<AgentRecord>::new(&db).get(&AgentId::from("a"));
            assert_eq!(result.is_ok(), valid, "{raw}");
            if valid {
                assert_eq!(
                    result.unwrap().unwrap().account_key,
                    raw["accountKey"].as_str().unwrap()
                );
            }
        }
    }

    #[test]
    fn checked_projection_refuses_schema_errors_and_wrong_row_identity_without_content() {
        let db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .expect("agents");
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile");
        for value in [
            json!({"compactions":0,"id":"a","rootId":"r","epoch":0,"status":"idle","activity":{"phase":{"secret-value":true}}}),
            json!({"compactions":0,"id":"wrong","rootId":"r","epoch":0,"status":"idle"}),
        ] {
            let error =
                decode_agent_record(&db, "a", &value.to_string()).expect_err("invalid agent");
            assert!(matches!(
                error,
                crate::StoreError::InvalidSyncObjectField { .. }
            ));
            assert!(!error.to_string().contains("secret-value"));
        }
    }
}
