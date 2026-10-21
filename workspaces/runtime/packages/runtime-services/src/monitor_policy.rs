//! Monitor decisions. Stored records enter only through `MonitorSnapshot::checked`.
//! No clock, store, process, or provider is consulted by these transitions.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub const TAIL_CHARS: usize = studio_runtime_schema::shared_values::MONITOR_OUTPUT_TAIL_CODEPOINTS;
pub const CHUNK_BYTES: usize = 65_536;
pub const PROBE_LEASE_SECONDS: f64 = 90.0;
pub const DEFAULT_MONITOR_STALL_SECONDS: u64 = 1800;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorStatus {
    Approval,
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

impl MonitorStatus {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Lost
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub agent: String,
    pub epoch: i64,
    pub account_key: String,
    pub connection_id: String,
    #[serde(flatten)]
    pub evidence: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Error)]
pub enum MonitorError {
    #[error("Invalid stored object monitors/{id} at {location}")]
    InvalidStored { id: String, location: String },
    #[error("Invalid persisted monitor result receipt: {0}")]
    InvalidResultReceipt(&'static str),
    #[error("Invalid monitor delivery identity: {0}")]
    InvalidDeliveryIdentity(&'static str),
    #[error("Invalid monitor output receipt: {0}")]
    InvalidOutputReceipt(&'static str),
    #[error("Monitor runtime tick task failed")]
    TaskFailed,
    #[error("{0}")]
    Refused(&'static str),
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error("{stage}: {source:?}")]
    ProviderRequest {
        stage: &'static str,
        source: crate::account_session_runtime::AccountSessionError,
    },
    #[error("Provider rejected monitor configuration ({code:?}): {message}")]
    ConfigurationRejected { code: Option<i64>, message: String },
    #[error("Monitor configuration receipt is {certainty:?} ({reason:?})")]
    ConfigurationReceipt {
        certainty: crate::account_session_runtime::RequestCertainty,
        reason: Option<crate::account_session_runtime::NotSentReason>,
    },
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    Event(#[from] crate::event_write::EventWriteError),
    #[error(transparent)]
    Rules(#[from] crate::rules_tick::RulesTickError),
    #[error("monitor file operation failed")]
    Io(#[from] std::io::Error),
}

impl MonitorError {
    /// A failure at the durable store boundary, retryable or not.
    pub fn store_failure_for_task(&self) -> bool {
        self.store_error().is_some()
    }

    /// Only a permanently failed durable store boundary may stop the retained
    /// runtime task. A retryable store failure, for example a full disk, holds
    /// the work and retries it from durable state.
    pub fn stops_runtime_task(&self) -> bool {
        self.store_error()
            .is_some_and(|error| !retryable_store_error(error))
    }

    /// A store or file failure that a later attempt of the same idempotent
    /// write can pass: a full disk, an unopenable database file, an I/O error,
    /// a busy database or a lost commit response. Invalid stored state, schema
    /// contracts and a lost state directory owner are not retryable.
    pub fn retryable_store_failure(&self) -> bool {
        matches!(self, Self::Io(_)) || self.store_error().is_some_and(retryable_store_error)
    }

    fn store_error(&self) -> Option<&studio_runtime_store::StoreError> {
        match self {
            Self::Store(error)
            | Self::Event(crate::event_write::EventWriteError::Store(error))
            | Self::Rules(crate::rules_tick::RulesTickError::Store(error))
            | Self::Rules(crate::rules_tick::RulesTickError::Event(
                crate::event_write::EventWriteError::Store(error),
            )) => Some(error),
            _ => None,
        }
    }

    pub fn is_bad_delivery(&self) -> bool {
        matches!(
            self,
            Self::InvalidDeliveryIdentity(_) | Self::InvalidOutputReceipt(_)
        )
    }

    /// Invalid persisted state and incompatible contracts cannot be repaired by
    /// replaying a timer batch. Other failures retain their durable identities.
    pub fn fatal_for_tick(&self) -> bool {
        match self {
            Self::InvalidStored { .. } => true,
            Self::InvalidResultReceipt(_) => true,
            Self::InvalidDeliveryIdentity(_) => true,
            Self::InvalidOutputReceipt(_) => true,
            Self::TaskFailed => true,
            Self::Store(error) => fatal_store_error(error),
            Self::Event(crate::event_write::EventWriteError::InvalidStoredObject { .. }) => true,
            Self::Event(crate::event_write::EventWriteError::Store(error)) => {
                fatal_store_error(error)
            }
            Self::Rules(crate::rules_tick::RulesTickError::Rule(
                crate::rules_engine::RuleError::Invalid { .. }
                | crate::rules_engine::RuleError::Schema,
            )) => true,
            Self::Rules(crate::rules_tick::RulesTickError::Store(error)) => {
                fatal_store_error(error)
            }
            Self::Rules(crate::rules_tick::RulesTickError::Event(
                crate::event_write::EventWriteError::InvalidStoredObject { .. },
            )) => true,
            Self::Rules(crate::rules_tick::RulesTickError::Event(
                crate::event_write::EventWriteError::Store(error),
            )) => fatal_store_error(error),
            _ => false,
        }
    }
}

fn retryable_store_error(error: &studio_runtime_store::StoreError) -> bool {
    matches!(
        error,
        studio_runtime_store::StoreError::Sqlite(_)
            | studio_runtime_store::StoreError::CommitOutcomeUnknown(_)
            | studio_runtime_store::StoreError::CommitActorOutcomeUnknown(_)
            | studio_runtime_store::StoreError::StateDirectory(_)
            | studio_runtime_store::StoreError::ReadTaskFailed
    )
}

fn fatal_store_error(error: &studio_runtime_store::StoreError) -> bool {
    matches!(
        error,
        studio_runtime_store::StoreError::SchemaMismatch(_)
            | studio_runtime_store::StoreError::InvalidSyncObject { .. }
            | studio_runtime_store::StoreError::InvalidStoredRecord { .. }
            | studio_runtime_store::StoreError::SchemaObjectMismatch { .. }
            | studio_runtime_store::StoreError::InvalidStoredObject { .. }
            | studio_runtime_store::StoreError::SchemaValidation(_)
            | studio_runtime_store::StoreError::RecordContract(_)
            | studio_runtime_store::StoreError::Json(_)
    )
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RecoveryStatus {
    Running,
    NotReattachable,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReattachRecovery {
    account_key: String,
    epoch: i64,
    status: RecoveryStatus,
    started_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proof: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fields {
    id: String,
    agent: String,
    #[serde(default)]
    epoch: Option<i64>,
    status: MonitorStatus,
    #[serde(default)]
    reattach_recovery: Option<ReattachRecovery>,
    #[serde(default)]
    rule_id: Option<String>,
    #[serde(default)]
    operation: Option<Operation>,
    #[serde(default)]
    cancel_requested: bool,
    #[serde(default)]
    interactive: bool,
    #[serde(default, rename = "tail")]
    _tail: String,
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    activity_generation: i64,
    #[serde(default = "minus_one")]
    stall_wake_generation: i64,
    #[serde(default)]
    stall_probe_generation: Option<i64>,
    #[serde(default)]
    stall_probe_started: Option<f64>,
    #[serde(default)]
    activity_at: Option<f64>,
    #[serde(default)]
    created: Option<f64>,
    #[serde(default = "stall_default")]
    stall_timeout_seconds: u64,
    #[serde(default)]
    liveness_command: String,
    #[serde(default = "success_default")]
    success_exit_codes: Vec<i64>,
    #[serde(default)]
    stdin_closed: bool,
    #[serde(default)]
    stdin_close_requested: Option<String>,
    #[serde(default)]
    input_attempt: Option<Value>,
}
fn minus_one() -> i64 {
    -1
}
fn stall_default() -> u64 {
    DEFAULT_MONITOR_STALL_SECONDS
}
fn success_default() -> Vec<i64> {
    vec![0]
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Actor {
    id: String,
    epoch: i64,
    auto_wake: bool,
    #[serde(default)]
    deleted_at: Option<f64>,
    #[serde(default = "default_account")]
    account_key: String,
}
fn default_account() -> String {
    "default".into()
}

#[derive(Clone)]
pub struct MonitorSnapshot {
    record: Value,
    fields: Fields,
    actor: Actor,
}

impl MonitorSnapshot {
    pub fn checked(record: Value, actor: Value) -> Result<Self, MonitorError> {
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let invalid = |location: &str| MonitorError::InvalidStored {
            id: id.clone(),
            location: location.into(),
        };
        if let Some((path, _)) =
            studio_runtime_schema::first_error("PersistedMonitorRecord", &record)
                .map_err(|_| invalid("schema"))?
        {
            return Err(invalid(&path));
        }
        if !studio_runtime_schema::validate("PersistedAgentRecord", &actor)
            .map_err(|_| invalid("agent"))?
        {
            return Err(invalid("agent"));
        }
        let fields: Fields =
            serde_json::from_value(record.clone()).map_err(|_| invalid("record"))?;
        let actor: Actor = serde_json::from_value(actor).map_err(|_| invalid("agent"))?;
        if fields.agent != actor.id {
            return Err(invalid("agent/id"));
        }
        if let Some(operation) = &fields.operation
            && (operation.agent != fields.agent
                || fields.epoch.is_some_and(|epoch| operation.epoch != epoch))
        {
            return Err(invalid("operation"));
        }
        Ok(Self {
            record,
            fields,
            actor,
        })
    }
    pub fn record(&self) -> &Value {
        &self.record
    }
    pub fn id(&self) -> &str {
        &self.fields.id
    }
    pub fn agent(&self) -> &str {
        &self.fields.agent
    }
    pub(crate) fn active(&self) -> bool {
        matches!(
            self.fields.status,
            MonitorStatus::Running | MonitorStatus::Starting | MonitorStatus::Approval
        )
    }
    /// No command bytes can be sent until reserve_start commits an operation.
    pub(crate) fn unsubmitted_intent(&self) -> bool {
        self.fields.operation.is_none()
            && matches!(
                self.fields.status,
                MonitorStatus::Starting | MonitorStatus::Approval
            )
    }
    pub(crate) fn retains_approval_for(&self, agent: &str) -> bool {
        self.fields.status == MonitorStatus::Approval
            && self.unsubmitted_intent()
            && self.agent() == agent
            && self.active_owner()
    }
    pub(crate) fn recovering(&self) -> bool {
        self.fields
            .reattach_recovery
            .as_ref()
            .is_some_and(|recovery| recovery.status == RecoveryStatus::Running)
    }
    pub(crate) fn reconciliation_deadline(&self) -> Option<f64> {
        self.fields
            .reattach_recovery
            .as_ref()
            .filter(|recovery| recovery.status == RecoveryStatus::Running)
            .map(|recovery| recovery.started_at + MONITOR_REATTACH_WINDOW_SECONDS)
    }
    pub fn operation(&self) -> Option<&Operation> {
        self.fields.operation.as_ref()
    }
    pub(crate) fn active_owner(&self) -> bool {
        self.actor.deleted_at.is_none_or(|at| at == 0.0)
            && self.actor.auto_wake
            && Some(self.actor.epoch)
                == self
                    .fields
                    .epoch
                    .or_else(|| self.operation().map(|operation| operation.epoch))
    }
    pub(crate) fn decision(&self) -> Decision {
        Decision {
            record: self.record.clone(),
            effects: vec![],
            response: Value::Null,
        }
    }
    fn check_operation(&self, operation: &Operation) -> Result<(), MonitorError> {
        if self.operation() != Some(operation) {
            return Err(MonitorError::Refused("Monitor operation identity changed"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    OutputEvent {
        id: String,
        payload: Value,
    },
    ExitEvent,
    StallEvent {
        generation: i64,
        payload: Value,
    },
    Probe {
        generation: i64,
        command: String,
    },
    ExpireApprovals,
    Terminate,
    Native {
        identity: String,
        method: &'static str,
        params: Value,
    },
    RuleFinished,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub record: Value,
    pub effects: Vec<Effect>,
    pub response: Value,
}

pub fn cancel(
    snapshot: &MonitorSnapshot,
    owner: Option<&str>,
    now: f64,
) -> Result<Decision, MonitorError> {
    if owner.is_some_and(|owner| owner != snapshot.agent()) {
        return Err(MonitorError::Refused(
            "This command belongs to another agent",
        ));
    }
    let f = &snapshot.fields;
    let mut decision = snapshot.decision();
    decision.response = json!({"id": f.id, "status": f.status});
    if !matches!(
        f.status,
        MonitorStatus::Running | MonitorStatus::Starting | MonitorStatus::Approval
    ) {
        return Ok(decision);
    }
    let running = f.status == MonitorStatus::Running;
    decision.response["cancelRequested"] = json!(running);
    if running && f.cancel_requested {
        return Ok(decision);
    }
    if running {
        decision.record["cancelRequested"] = json!(true);
        decision.effects.push(Effect::Terminate);
    } else {
        decision.record["status"] = json!("cancelled");
        decision.record["finished"] = json!(now);
        decision.record["configurationPending"] = json!(false);
        decision.response["status"] = json!("cancelled");
        decision
            .effects
            .push(if f.rule_id.as_deref().is_some_and(|id| !id.is_empty()) {
                Effect::RuleFinished
            } else {
                Effect::ExitEvent
            });
    }
    decision.effects.push(Effect::ExpireApprovals);
    Ok(decision)
}

pub fn output(
    snapshot: &MonitorSnapshot,
    operation: &Operation,
    chunk: &[u8],
    now: f64,
) -> Result<Decision, MonitorError> {
    output_stream(snapshot, operation, "stdout", chunk, now)
}

pub fn output_stream(
    snapshot: &MonitorSnapshot,
    operation: &Operation,
    stream: &str,
    chunk: &[u8],
    now: f64,
) -> Result<Decision, MonitorError> {
    snapshot.check_operation(operation)?;
    let mut decision = snapshot.decision();
    if snapshot.fields.status != MonitorStatus::Running {
        return Ok(decision);
    }
    if chunk.len() > CHUNK_BYTES {
        return Err(MonitorError::Refused("Monitor output chunk exceeds bound"));
    }
    let bytes = snapshot
        .fields
        .bytes
        .checked_add(chunk.len() as u64)
        .ok_or(MonitorError::Refused("Monitor output counter exhausted"))?;
    crate::monitor_matches::append(&mut decision.record, stream, chunk, snapshot.active_owner())?;
    output_effect(snapshot, &mut decision, now, false);
    decision.record["bytes"] = json!(bytes);
    if !chunk.is_empty() {
        decision.record["activityAt"] = json!(now);
        decision.record["activityGeneration"] = json!(
            snapshot
                .fields
                .activity_generation
                .checked_add(1)
                .ok_or(MonitorError::Refused("Monitor activity counter exhausted"))?
        );
    }
    Ok(decision)
}

pub fn finish(
    snapshot: &MonitorSnapshot,
    operation: Option<&Operation>,
    code: Option<i64>,
    error: Option<&str>,
    now: f64,
) -> Result<Decision, MonitorError> {
    if snapshot.operation() != operation {
        return Err(MonitorError::Refused(
            "Monitor result belongs to a different command operation",
        ));
    }
    if code.is_none() && error.is_none_or(str::is_empty) {
        return Err(MonitorError::Refused(
            "Monitor result has no definitive outcome",
        ));
    }
    if operation.is_none() && snapshot.fields.status == MonitorStatus::Running {
        return Err(MonitorError::Refused(
            "A submitted command requires a native operation identity",
        ));
    }
    let mut decision = snapshot.decision();
    if !snapshot.fields.status.terminal() {
        remove(&mut decision.record, "reattachRecovery");
        crate::monitor_matches::flush(&mut decision.record)?;
        output_effect(snapshot, &mut decision, now, true);
        decision.record["status"] = json!(if snapshot.fields.cancel_requested {
            MonitorStatus::Cancelled
        } else if error.is_some_and(|e| !e.is_empty())
            || !code.is_some_and(|c| snapshot.fields.success_exit_codes.contains(&c))
        {
            MonitorStatus::Failed
        } else {
            MonitorStatus::Completed
        });
        decision.record["exitCode"] = json!(code);
        decision.record["error"] = json!(error);
        decision.record["finished"] = json!(now);
        decision.record["configurationPending"] = json!(false);
        if snapshot.fields.cancel_requested && snapshot.record().get("cancelDispatch").is_some() {
            decision.record["cancelDispatch"] = json!({"status":"done","updatedAt":now});
        }
    }
    decision.effects.push(
        if snapshot
            .fields
            .rule_id
            .as_deref()
            .is_some_and(|id| !id.is_empty())
        {
            Effect::RuleFinished
        } else {
            Effect::ExitEvent
        },
    );
    Ok(decision)
}

fn output_effect(snapshot: &MonitorSnapshot, decision: &mut Decision, now: f64, final_chunk: bool) {
    if snapshot.fields.rule_id.is_none()
        && snapshot.active_owner()
        && let Some((id, payload)) =
            crate::monitor_matches::pending(&mut decision.record, now, final_chunk)
    {
        decision.effects.push(Effect::OutputEvent { id, payload });
    }
}

pub fn tick(snapshot: &MonitorSnapshot, now: f64) -> Result<Decision, MonitorError> {
    let f = &snapshot.fields;
    let mut decision = snapshot.decision();
    if snapshot.recovering() {
        return Ok(decision);
    }
    if f.status == MonitorStatus::Running {
        output_effect(snapshot, &mut decision, now, false);
    }
    let quiet = (now - f.activity_at.or(f.created).unwrap_or(now)).max(0.0) as u64;
    // A cancelled command waits only for its exit receipt; a stall notice would
    // wake an owner that already asked it to stop.
    if f.status != MonitorStatus::Running
        || f.cancel_requested
        || !snapshot.active_owner()
        || f.stall_timeout_seconds == 0
        || quiet < f.stall_timeout_seconds
        || f.activity_generation <= f.stall_wake_generation
    {
        return Ok(decision);
    }
    if f.stall_probe_generation == Some(f.activity_generation)
        && now - f.stall_probe_started.unwrap_or(now) < PROBE_LEASE_SECONDS
    {
        return Ok(decision);
    }
    decision.record["lastStallAt"] = json!(now);
    if f.liveness_command.is_empty() {
        decision.record["stallWakeGeneration"] = json!(f.activity_generation);
        remove(&mut decision.record, "stallProbeGeneration");
        decision.effects.push(Effect::StallEvent {
            generation: f.activity_generation,
            payload: stall_payload(snapshot.id(), quiet),
        });
    } else {
        decision.record["stallProbeGeneration"] = json!(f.activity_generation);
        decision.record["stallProbeStarted"] = json!(now);
        decision.effects.push(Effect::Probe {
            generation: f.activity_generation,
            command: f.liveness_command.clone(),
        });
    }
    Ok(decision)
}

pub fn probe_result(
    snapshot: &MonitorSnapshot,
    generation: i64,
    result: Value,
    now: f64,
) -> Result<Decision, MonitorError> {
    let mut decision = snapshot.decision();
    let f = &snapshot.fields;
    if f.status != MonitorStatus::Running
        || snapshot.actor.deleted_at.is_some_and(|at| at != 0.0)
        || Some(snapshot.actor.epoch) != f.epoch
        || f.stall_probe_generation != Some(generation)
        || f.activity_generation != generation
    {
        return Ok(decision);
    }
    let quiet = (now - f.activity_at.unwrap_or(now)).max(0.0) as u64;
    let mut payload = stall_payload(snapshot.id(), quiet);
    payload["livenessResult"] = result.clone();
    decision.record["stallWakeGeneration"] = json!(generation);
    decision.record["lastLivenessResult"] = result;
    remove(&mut decision.record, "stallProbeGeneration");
    decision.effects.push(Effect::StallEvent {
        generation,
        payload,
    });
    Ok(decision)
}

fn stall_payload(id: &str, quiet: u64) -> Value {
    json!({"id": id, "status": "running", "message": format!("no output for {} min; process still running", quiet / 60), "quietSeconds": quiet})
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    #[serde(default)]
    pub text: String,
    #[serde(default, deserialize_with = "present_dimension")]
    pub rows: Option<u16>,
    #[serde(default, deserialize_with = "present_dimension")]
    pub cols: Option<u16>,
    #[serde(default)]
    pub close_stdin: bool,
}

fn present_dimension<'de, D: serde::Deserializer<'de>>(input: D) -> Result<Option<u16>, D::Error> {
    u16::deserialize(input).map(Some)
}

pub fn begin_input(
    snapshot: &MonitorSnapshot,
    operation: &Operation,
    attempt_id: &str,
    input: &Input,
    now: f64,
) -> Result<Decision, MonitorError> {
    snapshot.check_operation(operation)?;
    let mut value = serde_json::to_value(input).map_err(studio_runtime_store::StoreError::from)?;
    value["id"] = json!(snapshot.id());
    if input.rows.is_none() {
        remove(&mut value, "rows");
    }
    if input.cols.is_none() {
        remove(&mut value, "cols");
    }
    if !studio_runtime_schema::validate("MonitorInput", &value)
        .map_err(|_| MonitorError::Refused("Invalid monitor input schema"))?
    {
        return Err(MonitorError::Refused("Invalid monitor input"));
    }
    let f = &snapshot.fields;
    if f.status != MonitorStatus::Running
        || !snapshot.active_owner()
        || f.cancel_requested
        || !f.interactive
        || snapshot.actor.account_key != operation.account_key
    {
        return Err(MonitorError::Refused(
            "This interactive monitor is not active",
        ));
    }
    if f.input_attempt.is_some() {
        return Err(MonitorError::Refused(
            "The previous monitor input outcome is unknown; do not repeat it",
        ));
    }
    if snapshot
        .record
        .pointer("/lastInputAttempt/id")
        .and_then(Value::as_str)
        == Some(attempt_id)
    {
        return Err(MonitorError::Refused(
            "Monitor input ticket already settled",
        ));
    }
    let resize = input.rows.is_some() || input.cols.is_some();
    let (method, params) = if resize {
        (
            "command/exec/resize",
            json!({"processId": f.id, "size": {"rows": input.rows.unwrap_or(24), "cols": input.cols.unwrap_or(80)}}),
        )
    } else {
        if f.stdin_closed || f.stdin_close_requested.is_some() {
            return Err(MonitorError::Refused(
                "This monitor stdin is closed or its close acknowledgement is pending",
            ));
        }
        (
            "command/exec/write",
            json!({"processId": f.id, "deltaBase64": base64::engine::general_purpose::STANDARD.encode(input.text.as_bytes()), "closeStdin": input.close_stdin}),
        )
    };
    let mut attempt = serde_json::to_value(operation)
        .map_err(|_| MonitorError::Refused("Invalid monitor identity"))?;
    attempt["id"] = json!(attempt_id);
    attempt["method"] = json!(method);
    attempt["params"] = params.clone();
    attempt["status"] = json!("submitted");
    attempt["created"] = json!(now);
    let mut decision = snapshot.decision();
    decision.record["inputAttempt"] = attempt;
    if !resize && input.close_stdin {
        decision.record["stdinCloseRequested"] = json!(attempt_id);
        remove(&mut decision.record, "stdinError");
    }
    decision.effects.push(Effect::Native {
        identity: format!("monitor-input:{}:{attempt_id}", f.id),
        method,
        params,
    });
    Ok(decision)
}

#[derive(Clone)]
pub enum InputOutcome {
    Acknowledged,
    Rejected(String),
    Unknown(String),
}

pub fn settle_input(
    snapshot: &MonitorSnapshot,
    operation: &Operation,
    attempt_id: &str,
    outcome: InputOutcome,
    now: f64,
) -> Result<Decision, MonitorError> {
    snapshot.check_operation(operation)?;
    let mut decision = snapshot.decision();
    if !snapshot.active_owner()
        || snapshot.fields.status != MonitorStatus::Running
        || snapshot.fields.cancel_requested
    {
        return Ok(decision);
    }
    let Some(attempt) = &snapshot.fields.input_attempt else {
        return Ok(decision);
    };
    if attempt.get("id").and_then(Value::as_str) != Some(attempt_id)
        || ["agent", "epoch", "accountKey", "connectionId"]
            .iter()
            .any(|key| attempt.get(*key) != snapshot.record.pointer(&format!("/operation/{key}")))
    {
        return Err(MonitorError::Refused(
            "Monitor input receipt identity changed",
        ));
    }
    let mut receipt = attempt.clone();
    receipt["finished"] = json!(now);
    receipt["status"] = json!(match outcome {
        InputOutcome::Acknowledged => "acknowledged",
        InputOutcome::Rejected(_) => "failed",
        InputOutcome::Unknown(_) => "uncertain",
    });
    let error = match &outcome {
        InputOutcome::Acknowledged => None,
        InputOutcome::Rejected(error) | InputOutcome::Unknown(error) => Some(error),
    };
    if let Some(error) = error {
        if error.chars().count() > 12000 {
            return Err(MonitorError::Refused("Monitor input error exceeds bound"));
        }
        receipt["error"] = json!(error);
    }
    if matches!(outcome, InputOutcome::Unknown(_)) {
        decision.record["inputAttempt"] = receipt;
    } else {
        remove(&mut decision.record, "inputAttempt");
        decision.record["lastInputAttempt"] = receipt;
    }
    if snapshot.fields.stdin_close_requested.as_deref() == Some(attempt_id) {
        if !matches!(outcome, InputOutcome::Unknown(_)) {
            remove(&mut decision.record, "stdinCloseRequested");
        }
        if let Some(error) = error {
            decision.record["stdinError"] = json!(error);
        }
        if matches!(outcome, InputOutcome::Acknowledged) {
            decision.record["stdinClosed"] = json!(true);
            remove(&mut decision.record, "stdinError");
        }
    }
    Ok(decision)
}

fn remove(value: &mut Value, field: &str) {
    if let Some(object) = value.as_object_mut() {
        object.remove(field);
    }
}

/// Preparation supplied this exact operation and native command. A changed snapshot is
/// refused by the transactional caller before this durable submission intent is written.
pub fn start(
    snapshot: &MonitorSnapshot,
    operation: &Operation,
    params: Value,
    now: f64,
) -> Result<Decision, MonitorError> {
    if snapshot.fields.status != MonitorStatus::Starting
        || !snapshot.active_owner()
        || operation.agent != snapshot.agent()
        || operation.epoch != snapshot.actor.epoch
        || operation.account_key != snapshot.actor.account_key
        || operation.connection_id.is_empty()
    {
        return Err(MonitorError::Refused("Monitor start ownership changed"));
    }
    if params.get("processId").and_then(Value::as_str) != Some(snapshot.id())
        || params.get("cwd") != snapshot.record.get("cwd")
        || params.get("timeoutMs") != snapshot.record.get("timeout_ms")
        || !(params.get("sandboxPolicy").is_some_and(Value::is_object)
            || params
                .get("permissionProfile")
                .is_some_and(Value::is_string))
    {
        return Err(MonitorError::Refused(
            "Monitor preparation does not match its command",
        ));
    }
    let mut decision = snapshot.decision();
    decision.record["operation"] =
        serde_json::to_value(operation).map_err(studio_runtime_store::StoreError::from)?;
    decision.record["status"] = json!("running");
    decision.record["error"] = Value::Null;
    decision.record["configurationPending"] = json!(false);
    decision.record["activityAt"] = json!(now);
    decision.record["activityGeneration"] = json!(0);
    decision.record["stallWakeGeneration"] = json!(-1);
    decision.effects.push(Effect::Native {
        identity: format!("monitor:{}", snapshot.id()),
        method: "command/exec",
        params,
    });
    Ok(decision)
}

/// How long after this backend starts a lost monitor may still be reattached to its
/// surviving provider operation. Owner: monitor reattach recovery. Unit: seconds.
/// Afterwards each remaining recovery settles as `not_reattachable` once.
pub const MONITOR_REATTACH_WINDOW_SECONDS: f64 = 600.0;

/// A supervisor's operation receipt is evidence of the original command; it is never
/// permission to run it again. The connection owner supplies this verified receipt.
pub struct ReattachProof {
    pub operation: Operation,
    pub operation_id: String,
    pub native_id: u64,
    pub current_connection: String,
}

/// A retained-process observation never launches a command.
pub enum ReattachOutcome {
    Pending,
    Running(ReattachProof),
    TerminalQueued,
    Gone(String),
}

/// Keep commands active while the new backend reconciles their retained process.
/// No completion is published until a native result or a failed reattach decides it.
pub fn restart_recovery(snapshot: &MonitorSnapshot, now: f64) -> Result<Decision, MonitorError> {
    if !matches!(
        snapshot.fields.status,
        MonitorStatus::Running | MonitorStatus::Starting | MonitorStatus::Approval
    ) {
        return Err(MonitorError::Refused("Monitor is not active"));
    }
    let mut decision = snapshot.decision();
    if snapshot.recovering() {
        return Ok(decision);
    }
    decision.record["reattachRecovery"] = json!(ReattachRecovery {
        account_key: snapshot.actor.account_key.clone(),
        epoch: snapshot.actor.epoch,
        status: RecoveryStatus::Running,
        started_at: now,
        proof: None,
    });
    Ok(decision)
}

/// A failed or expired reattach is the terminal loss decision, not a provisional notice.
pub(crate) fn restart_lost(
    snapshot: &MonitorSnapshot,
    reason: &str,
    now: f64,
) -> Result<Decision, MonitorError> {
    let mut decision = snapshot.decision();
    let Some(mut recovery) = snapshot
        .fields
        .reattach_recovery
        .clone()
        .filter(|recovery| recovery.status == RecoveryStatus::Running)
    else {
        return Ok(decision);
    };
    recovery.status = RecoveryStatus::NotReattachable;
    recovery.proof = Some(reason.into());
    decision.record["reattachRecovery"] = json!(recovery);
    decision.record["status"] = json!(MonitorStatus::Lost);
    decision.record["finished"] = json!(now);
    decision.record["error"] =
        json!("Command outcome unknown after losing the provider result; not rerun.");
    decision.effects.push(if snapshot.fields.rule_id.is_some() {
        Effect::RuleFinished
    } else {
        Effect::ExitEvent
    });
    Ok(decision)
}

pub fn reattach(
    snapshot: &MonitorSnapshot,
    proof: &ReattachProof,
) -> Result<Decision, MonitorError> {
    snapshot.check_operation(&proof.operation)?;
    let recovery = snapshot.fields.reattach_recovery.as_ref();
    if !matches!(
        snapshot.fields.status,
        MonitorStatus::Running | MonitorStatus::Starting | MonitorStatus::Approval
    ) || proof.operation_id != format!("monitor:{}", snapshot.id())
        || proof.current_connection.is_empty()
        || proof.operation.account_key != snapshot.actor.account_key
        || recovery.is_none_or(|recovery| {
            recovery.status != RecoveryStatus::Running
                || recovery.epoch != snapshot.actor.epoch
                || recovery.account_key != snapshot.actor.account_key
        })
    {
        return Err(MonitorError::Refused(
            "Monitor reattach proof does not match its command",
        ));
    }
    let mut decision = snapshot.decision();
    decision.record["status"] = json!("running");
    decision.record["reattachedConnectionId"] = json!(proof.current_connection);
    remove(&mut decision.record, "reattachRecovery");
    if !snapshot.active_owner() {
        // A verified survivor cannot resume an inactive owner. Latch the ordinary
        // cancellation path so deletion or an epoch change cannot orphan it.
        decision.record["cancelRequested"] = json!(true);
        decision.effects.push(Effect::Terminate);
    }
    Ok(decision)
}

pub enum ReattachDisposition {
    AwaitExit,
    Cancel,
}

/// Build the same initial row as Python. Identity, actor and time are server-owned;
/// request fields are schema-checked by the resulting persisted projection.
pub fn create_record(
    actor: &Value,
    request: &Value,
    request_id: &str,
    log_root: &str,
    now: f64,
) -> Result<Value, MonitorError> {
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, request_id.as_bytes()).to_string();
    let agent = actor
        .get("id")
        .and_then(Value::as_str)
        .ok_or(MonitorError::Refused("Unknown agent"))?;
    let command = request
        .get("command")
        .and_then(Value::as_str)
        .ok_or(MonitorError::Refused("Supply a command"))?;
    if command.trim().is_empty() {
        return Err(MonitorError::Refused("Supply a command"));
    }
    let approved = actor.get("yoloMode").filter(|v| !v.is_null()).map_or_else(
        || actor.get("approvalPolicy").and_then(Value::as_str) == Some("never"),
        |v| v == &Value::Bool(true),
    );
    let success = request
        .get("success_exit_codes")
        .cloned()
        .unwrap_or(json!([0]));

    let output_patterns = crate::monitor_matches::patterns(request.get("output_patterns"))?;
    let mut record = json!({
        "outputPatterns": output_patterns,
        "outputEventSequence": 0, "outputEventCount": 0, "outputEventIds": [],
        "id": id, "agent": agent, "epoch": actor.get("epoch"), "turnId": actor.get("turnId"), "requestId": request_id,
        "command": command, "cwd": actor.get("cwd"), "timeout_ms": request.get("timeout_ms").cloned().unwrap_or(json!(3600000)),
        "stallTimeoutSeconds": request.get("stall_timeout_seconds").cloned().unwrap_or(json!(1800)),
        "livenessCommand": request.get("liveness_command").cloned().unwrap_or(json!("")), "activityAt": now,
        "activityGeneration": 0, "stallWakeGeneration": -1, "wakeOn": request.get("wake_on").cloned().unwrap_or(json!("exit")),
        "successExitCodes": success, "status": if approved { "starting" } else { "approval" }, "created": now,
        "exitCode": null, "tail": "", "bytes": 0, "interactive": request.get("interactive").cloned().unwrap_or(json!(false)),
        "ruleId": null, "log": format!("{log_root}/{id}.log")
    });
    MonitorSnapshot::checked(record.clone(), actor.clone())?;
    if let Some(codes) = record["successExitCodes"].as_array_mut() {
        codes.sort_by_key(Value::as_i64);
        codes.dedup();
    }
    if let Some(command) = record["livenessCommand"].as_str() {
        record["livenessCommand"] = json!(command.trim());
    }
    MonitorSnapshot::checked(record.clone(), actor.clone())?;
    Ok(record)
}

pub fn same_request(previous: &Value, proposed: &Value) -> bool {
    [
        "agent",
        "command",
        "timeout_ms",
        "interactive",
        "wakeOn",
        "successExitCodes",
        "stallTimeoutSeconds",
        "livenessCommand",
    ]
    .iter()
    .all(|field| previous.get(*field) == proposed.get(*field))
        && previous.get("outputPatterns").cloned().unwrap_or(json!([]))
            == proposed.get("outputPatterns").cloned().unwrap_or(json!([]))
}
