//! Typed persisted monitor rows and their open transport evidence.
use crate::studio_errors::MonitorStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{borrow::Cow, collections::BTreeMap};

pub use crate::bounded_integer::BoundedInteger as MonitorInteger;
pub use crate::bounded_text::BoundedText as MonitorText;
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MonitorTime(f64);
impl MonitorTime {
    pub fn seconds(&self) -> f64 {
        self.0
    }
}
impl TryFrom<f64> for MonitorTime {
    type Error = &'static str;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value.is_finite() && value >= 0.0 {
            Ok(Self(value))
        } else {
            Err("invalid monitor time")
        }
    }
}
impl<'de> Deserialize<'de> for MonitorTime {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(f64::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl schemars::JsonSchema for MonitorTime {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "MonitorTime".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"number","minimum":0})
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MonitorExitCodes(Vec<MonitorInteger<0, 255>>);
impl MonitorExitCodes {
    pub fn values(&self) -> &[MonitorInteger<0, 255>] {
        &self.0
    }
}
impl TryFrom<Vec<i64>> for MonitorExitCodes {
    type Error = &'static str;
    fn try_from(mut values: Vec<i64>) -> Result<Self, Self::Error> {
        if !(1..=16).contains(&values.len()) {
            return Err("invalid exit code count");
        }
        values.sort_unstable();
        values.dedup();
        Ok(Self(
            values
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        ))
    }
}
impl<'de> Deserialize<'de> for MonitorExitCodes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let items = Vec::deserialize(d)?;
        if (1..=16).contains(&items.len()) {
            Ok(Self(items))
        } else {
            Err(serde::de::Error::custom("invalid exit code count"))
        }
    }
}
impl schemars::JsonSchema for MonitorExitCodes {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "MonitorExitCodes".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"array","minItems":1,"maxItems":16,"items":{"type":"integer","minimum":0,"maximum":255}})
    }
}
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MonitorWakeOn {
    Exit,
    Failure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorOperationRecord {
    pub account_key: MonitorText<1, 200>,
    pub agent: MonitorText<1, 200>,
    pub connection_id: MonitorText<1, 200>,
    pub epoch: MonitorInteger<0, 9007199254740991>,
    #[serde(flatten)]
    pub evidence: MonitorOperationEvidence,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorOperationEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request: Option<crate::persistence::AdmittedRequest>,
}
impl MonitorOperationRecord {
    /// Historical thread id is retained but is not part of operation identity.
    pub fn same_identity(&self, other: &Self) -> bool {
        self.agent == other.agent
            && self.epoch == other.epoch
            && self.account_key == other.account_key
            && self.connection_id == other.connection_id
            && self.evidence.root_request_id == other.evidence.root_request_id
            && self.evidence.provider_generation == other.evidence.provider_generation
            && self.evidence.provider_request == other.evidence.provider_request
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInputAttempt {
    pub id: String,
    #[serde(flatten)]
    pub operation: MonitorOperationRecord,
    #[serde(flatten)]
    pub method: MonitorInputMethod,
    pub status: MonitorInputStatus,
    pub created: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub response: Option<crate::raw_json::RawJson>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum MonitorInputMethod {
    #[serde(rename = "command/exec/write")]
    Write(MonitorWriteParams),
    #[serde(rename = "command/exec/resize")]
    Resize(MonitorResizeParams),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MonitorWriteParams {
    pub process_id: String,
    pub delta_base64: String,
    pub close_stdin: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MonitorResizeParams {
    pub process_id: String,
    pub size: MonitorSize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorSize {
    pub rows: u16,
    pub cols: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MonitorInputStatus {
    Submitted,
    Acknowledged,
    Failed,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReattachRecovery {
    pub account_key: String,
    pub epoch: i64,
    #[serde(flatten)]
    pub state: ReattachState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ReattachState {
    Running {
        started_at: f64,
    },
    NotReattachable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<f64>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum MonitorQueueEvent {
    Output {
        cursor: String,
        offset: u64,
        byte_length: u64,
        delta_base64: String,
        stream: MonitorStream,
        operation: MonitorOperationRecord,
    },
    Probe {
        cursor: String,
        generation: i64,
        result: MonitorProbeResult,
        byte_length: u64,
        operation: MonitorOperationRecord,
    },
    Exit {
        cursor: String,
        code: Option<i64>,
        error: Option<String>,
        finished: f64,
        byte_length: u64,
        operation: MonitorOperationRecord,
    },
}
impl MonitorQueueEvent {
    pub fn cursor(&self) -> &str {
        match self {
            Self::Output { cursor, .. }
            | Self::Probe { cursor, .. }
            | Self::Exit { cursor, .. } => cursor,
        }
    }
    pub fn byte_length(&self) -> u64 {
        match self {
            Self::Output { byte_length, .. }
            | Self::Probe { byte_length, .. }
            | Self::Exit { byte_length, .. } => *byte_length,
        }
    }
    pub fn operation(&self) -> &MonitorOperationRecord {
        match self {
            Self::Output { operation, .. }
            | Self::Probe { operation, .. }
            | Self::Exit { operation, .. } => operation,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MonitorStream {
    Stdout,
    Stderr,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum MonitorProbeResult {
    Completed {
        #[serde(rename = "exitCode")]
        exit_code: i64,
        output: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutputMatchExcerpt {
    pub excerpt: String,
    pub log_offset: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StreamValues<T> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<T>,
}
impl<T> Default for StreamValues<T> {
    fn default() -> Self {
        Self {
            stdout: None,
            stderr: None,
        }
    }
}
impl<T> StreamValues<T> {
    pub fn get(&self, stream: MonitorStream) -> Option<&T> {
        match stream {
            MonitorStream::Stdout => self.stdout.as_ref(),
            MonitorStream::Stderr => self.stderr.as_ref(),
        }
    }
    pub fn slot(&mut self, stream: MonitorStream) -> &mut Option<T> {
        match stream {
            MonitorStream::Stdout => &mut self.stdout,
            MonitorStream::Stderr => &mut self.stderr,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutputWakeBudget {
    pub tokens: f64,
    pub updated_at: f64,
    pub suppressed_matches: u64,
    pub total_suppressed_matches: u64,
    pub paused: bool,
    pub flood_since: Option<f64>,
    pub last_suppressed_at: Option<f64>,
    pub notice_sent: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorCancelDispatch {
    pub status: MonitorCancelStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MonitorCancelStatus {
    Pending,
    Written,
    Done,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LivenessProbe {
    pub generation: i64,
    pub root_request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request: Option<crate::persistence::AdmittedRequest>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ShellPressureCleanup {
    pub at: f64,
    pub generation: i64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeliveryFailure {
    pub cursor: String,
    pub error: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorTaskGate {
    pub task_id: String,
    pub result_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorProbeLease {
    pub stall_probe_generation: MonitorInteger<0, 9007199254740991>,
    pub stall_probe_started: MonitorTime,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ProbeFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stall_probe_generation: Option<MonitorInteger<0, 9007199254740991>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stall_probe_started: Option<MonitorTime>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(try_from = "ProbeFields", into = "ProbeFields")]
#[schemars(with = "ProbeFields")]
pub struct MonitorProbeState(pub Option<MonitorProbeLease>);
impl TryFrom<ProbeFields> for MonitorProbeState {
    type Error = &'static str;
    fn try_from(fields: ProbeFields) -> Result<Self, Self::Error> {
        match (fields.stall_probe_generation, fields.stall_probe_started) {
            (None, None) => Ok(Self(None)),
            (Some(stall_probe_generation), Some(stall_probe_started)) => {
                Ok(Self(Some(MonitorProbeLease {
                    stall_probe_generation,
                    stall_probe_started,
                })))
            }
            _ => Err("incomplete monitor probe lease"),
        }
    }
}
impl From<MonitorProbeState> for ProbeFields {
    fn from(value: MonitorProbeState) -> Self {
        match value.0 {
            Some(lease) => Self {
                stall_probe_generation: Some(lease.stall_probe_generation),
                stall_probe_started: Some(lease.stall_probe_started),
            },
            None => Self {
                stall_probe_generation: None,
                stall_probe_started: None,
            },
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitorRecord {
    pub activity_at: MonitorTime,
    pub activity_generation: MonitorInteger<0, 9007199254740991>,
    pub agent: String,
    pub bytes: MonitorInteger<0, 9007199254740991>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub cancel_requested: Option<bool>,
    pub command: MonitorText<0, 12000>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub configuration_pending: Option<bool>,
    pub created: MonitorTime,
    pub cwd: String,
    pub epoch: MonitorInteger<0, 9007199254740991>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<MonitorText<0, 12000>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<MonitorTime>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_attempt: Option<MonitorInputAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_attempt: Option<MonitorInputAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reattach_recovery: Option<ReattachRecovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_queue: Option<Vec<MonitorQueueEvent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_ack_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_dispatch: Option<MonitorCancelDispatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_provider_request: Option<crate::persistence::AdmittedRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_root_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reattached_connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stall_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_answer_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness_probe: Option<LivenessProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_liveness_result: Option<MonitorProbeResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_pressure_cleanup: Option<ShellPressureCleanup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_failure: Option<DeliveryFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_gate: Option<MonitorTaskGate>,
    pub request_id: String,
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_event_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_event_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_event_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_patterns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_line_offsets: Option<StreamValues<Vec<Option<u64>>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_line_tail: Option<StreamValues<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_match_excerpts: Option<BTreeMap<String, OutputMatchExcerpt>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_matcher_tail: Option<StreamValues<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_matches_pending: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_matches_total: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_utf8_pending: Option<StreamValues<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_wake_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_wake_budget: Option<OutputWakeBudget>,
    pub interactive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i64>,
    pub liveness_command: MonitorText<0, 12000>,
    pub log: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<MonitorOperationRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(flatten)]
    pub stall_probe: MonitorProbeState,
    pub stall_timeout_seconds: MonitorInteger<0, 31536000>,
    pub stall_wake_generation: MonitorInteger<-1, 9007199254740991>,
    pub status: MonitorStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_close_requested: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub stdin_closed: Option<bool>,
    pub success_exit_codes: MonitorExitCodes,
    pub tail: MonitorText<0, 12000>,
    #[serde(rename = "timeout_ms")]
    pub timeout_ms: MonitorInteger<1000, 86400000>,
    pub wake_on: MonitorWakeOn,
}
impl MonitorRecord {
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn agent(&self) -> &str {
        &self.agent
    }
    pub fn status(&self) -> MonitorStatus {
        self.status
    }
}
pub(crate) fn schema() -> Value {
    schemars::schema_for!(MonitorRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-monitor-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err("persisted monitor schema must match monitor_record::MonitorRecord".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_attempt_decodes_operation_and_method_once() {
        let raw = r#"{"id":"attempt","agent":"a","epoch":1,"accountKey":"account","connectionId":"connection","rootRequestId":"root","method":"command/exec/write","params":{"processId":"m","deltaBase64":"YQ==","closeStdin":false},"status":"submitted","created":1,"response":{"ok":true}}"#;
        let input: MonitorInputAttempt = serde_json::from_str(raw).unwrap();
        assert!(matches!(input.method, MonitorInputMethod::Write(_)));
        assert_eq!(input.operation.agent.as_str(), "a");
        assert_eq!(
            input.operation.evidence.root_request_id.as_deref(),
            Some("root")
        );
        assert_eq!(input.response.unwrap().0.get(), r#"{"ok":true}"#);
    }
    #[test]
    fn running_reattach_requires_start_time() {
        assert!(
            serde_json::from_str::<ReattachRecovery>(
                r#"{"accountKey":"a","epoch":1,"status":"running"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ReattachRecovery>(
                r#"{"accountKey":"a","epoch":1,"status":"running","startedAt":1}"#
            )
            .is_ok()
        );
    }
    #[test]
    fn probe_lease_requires_both_fields() {
        #[derive(Deserialize)]
        struct Record {
            #[serde(flatten)]
            probe: MonitorProbeState,
        }
        let complete = r#"{"stallProbeGeneration":1,"stallProbeStarted":2}"#;
        assert!(
            serde_json::from_str::<Record>(complete)
                .unwrap()
                .probe
                .0
                .is_some()
        );
        assert!(serde_json::from_str::<Record>(r#"{"stallProbeGeneration":1}"#).is_err());
    }
    #[test]
    fn strict_monitor_requires_owner_status_and_discards_extensions() {
        assert!(serde_json::from_str::<MonitorRecord>(r#"{"id":"m"}"#).is_err());
        let raw = r#"{"id":"m","agent":"a","epoch":1,"status":"running","requestId":"request","activityAt":1,"activityGeneration":0,"bytes":0,"command":"echo","created":1,"cwd":"fixture","interactive":false,"livenessCommand":"","log":"fixture-log","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit","unknown":true}"#;
        let record: MonitorRecord = serde_json::from_str(raw).unwrap();
        assert!(!serde_json::to_string(&record).unwrap().contains("unknown"));
    }
    #[test]
    fn stored_monitor_schema_matches_type() {
        check_schema_parity().unwrap();
    }
    #[test]
    fn operation_identity_is_bounded() {
        assert!(
            serde_json::from_str::<MonitorOperationRecord>(
                r#"{"agent":"","epoch":1,"accountKey":"a","connectionId":"c"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<MonitorOperationRecord>(
                r#"{"agent":"a","epoch":-1,"accountKey":"a","connectionId":"c"}"#
            )
            .is_err()
        );
    }
}
