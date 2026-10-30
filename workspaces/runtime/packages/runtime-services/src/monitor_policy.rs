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

fn monitor_time(value: f64) -> Result<studio_runtime_schema::monitor_record::MonitorTime, MonitorError> {
    value.try_into().map_err(|_| MonitorError::Refused("Invalid monitor time"))
}
fn monitor_integer<const MIN: i64, const MAX: u64>(value: i64) -> Result<studio_runtime_schema::monitor_record::MonitorInteger<MIN, MAX>, MonitorError> {
    value.try_into().map_err(|_| MonitorError::Refused("Monitor counter exhausted"))
}
fn monitor_text(value: &str) -> Result<studio_runtime_schema::monitor_record::MonitorText<0, 12000>, MonitorError> {
    value.to_owned().try_into().map_err(|_| MonitorError::Refused("Monitor text exceeds bound"))
}

use studio_runtime_schema::monitor_record::{ReattachRecovery, ReattachState};
pub use studio_runtime_schema::studio_errors::MonitorStatus;

pub use studio_runtime_store::{
    MonitorOperation as Operation, MonitorOperationEvidence as OperationEvidence,
};

#[derive(Debug, Error)]
pub enum MonitorError {
    #[error("Request id expired")]
    Expired,
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
    #[error("Monitor input outcome unknown; do not repeat it")]
    InputOutcomeUnknown,
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

impl From<studio_runtime_schema::persistence::RequestIdentityError> for MonitorError {
    fn from(error: studio_runtime_schema::persistence::RequestIdentityError) -> Self {
        Self::Store(studio_runtime_store::StoreError::RequestResult(
            error.into(),
        ))
    }
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
            Self::InvalidStored { .. }
            | Self::InvalidResultReceipt(_)
            | Self::InvalidDeliveryIdentity(_)
            | Self::InvalidOutputReceipt(_)
            | Self::TaskFailed
            | Self::Expired
            | Self::InputOutcomeUnknown
            | Self::Refused(_)
            | Self::Unavailable(_)
            | Self::ProviderRequest { .. }
            | Self::ConfigurationRejected { .. }
            | Self::ConfigurationReceipt { .. }
            | Self::Event(_)
            | Self::Rules(_)
            | Self::Io(_) => None,
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
            Self::Refused(_)
            | Self::Expired
            | Self::InputOutcomeUnknown
            | Self::Unavailable(_)
            | Self::ProviderRequest { .. }
            | Self::ConfigurationRejected { .. }
            | Self::ConfigurationReceipt { .. }
            | Self::Event(_)
            | Self::Rules(_)
            | Self::Io(_) => false,
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
    record: studio_runtime_store::MonitorRecord,
    actor: Actor,
}

impl MonitorSnapshot {
    #[cfg(test)]
    pub fn checked(record: Value, actor: Value) -> Result<Self, MonitorError> {
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let typed =
            serde_path_to_error::deserialize::<_, studio_runtime_store::MonitorRecord>(record)
                .map_err(|error| MonitorError::InvalidStored {
                    id,
                    location: format!("/{}", error.path()),
                })?;
        Self::checked_stored(&typed, actor)
    }

    pub fn checked_stored(
        typed: &studio_runtime_store::MonitorRecord,
        actor: Value,
    ) -> Result<Self, MonitorError> {
        let id = typed.id.clone();
        let invalid = |location: &str| MonitorError::InvalidStored {
            id: id.clone(),
            location: location.into(),
        };
        if !studio_runtime_schema::validate("PersistedAgentRecord", &actor)
            .map_err(|_| invalid("agent"))?
        {
            return Err(invalid("agent"));
        }
        crate::monitor_matches::validate(typed).map_err(invalid)?;
        let actor: Actor = serde_json::from_value(actor).map_err(|_| invalid("agent"))?;
        if typed.agent != actor.id {
            return Err(invalid("agent/id"));
        }
        if let Some(operation) = &typed.operation
            && (operation.agent.as_str() != typed.agent
                || operation.epoch.get() != typed.epoch.get())
        {
            return Err(invalid("operation"));
        }
        Ok(Self {
            record: typed.clone(),
            actor,
        })
    }
    pub fn record(&self) -> &studio_runtime_store::MonitorRecord {
        &self.record
    }
    pub fn id(&self) -> &str {
        &self.record.id
    }
    pub fn status(&self) -> MonitorStatus {
        self.record.status
    }
    pub fn rule_id(&self) -> Option<&str> {
        self.record.rule_id.as_deref()
    }
    pub fn agent(&self) -> &str {
        &self.record.agent
    }
    /// Only these fields can change selection in the canonical monitor window.
    pub(crate) fn same_window_position(&self, other: &Self) -> bool {
        self.record.id == other.record.id
            && self.record.agent == other.record.agent
            && self.record.status == other.record.status
            && self.record.created == other.record.created
    }
    pub(crate) fn live_window_owner(&self) -> bool {
        self.actor.deleted_at.is_none()
    }
    pub(crate) fn active(&self) -> bool {
        matches!(
            self.record.status,
            MonitorStatus::Running | MonitorStatus::Starting | MonitorStatus::Approval
        )
    }
    /// No command bytes can be sent until reserve_start commits an operation.
    pub(crate) fn unsubmitted_intent(&self) -> bool {
        self.record.operation.is_none()
            && matches!(
                self.record.status,
                MonitorStatus::Starting | MonitorStatus::Approval
            )
    }
    pub(crate) fn retains_approval_for(&self, agent: &str) -> bool {
        self.record.status == MonitorStatus::Approval
            && self.unsubmitted_intent()
            && self.agent() == agent
            && self.active_owner()
    }
    pub(crate) fn recovering(&self) -> bool {
        self.record
            .reattach_recovery
            .as_ref()
            .is_some_and(|recovery| matches!(recovery.state, ReattachState::Running { .. }))
    }
    /// A loss decided at the command deadline. The provider process may still
    /// send the command's real answer, which then completes it once.
    pub(crate) fn deadline_lost(&self) -> bool {
        self.record.status == MonitorStatus::Lost
            && self.record.rule_id.as_deref().is_none_or(str::is_empty)
            && self
                .record
                .reattach_recovery
                .as_ref()
                .is_some_and(|recovery| recovery.proof.as_deref() == Some(DEADLINE_PASSED))
    }
    /// A deadline loss whose late answer is still awaited at `now`.
    pub(crate) fn awaits_late_answer(&self, now: f64) -> bool {
        self.deadline_lost()
            && self.record.finished.as_ref().is_some_and(|finished| now < finished.seconds() + MONITOR_LATE_ANSWER_SECONDS)
    }
    pub(crate) fn reconciliation_deadline(&self) -> Option<f64> {
        self.record
            .reattach_recovery
            .as_ref()
            .filter(|recovery| matches!(recovery.state, ReattachState::Running { .. }))
            .and_then(|recovery| match recovery.state {
                ReattachState::Running { started_at } => {
                    Some(started_at + MONITOR_REATTACH_WINDOW_SECONDS)
                }
                ReattachState::NotReattachable { .. } => None,
            })
    }
    pub fn operation(&self) -> Option<&Operation> {
        self.record.operation.as_ref()
    }
    /// The native process stays on its submitted account after a model-account move.
    fn command_account(&self) -> &str {
        self.operation()
            .map(|operation| operation.account_key.as_str())
            .unwrap_or(&self.actor.account_key)
    }
    pub(crate) fn active_owner(&self) -> bool {
        self.actor.deleted_at.is_none_or(|at| at == 0.0)
            && self.actor.auto_wake
            && self.actor.epoch == self.record.epoch.get()
    }
    pub(crate) fn decision(&self) -> Decision {
        Decision {
            record: self.record.clone(),
            effects: vec![],
            response: Value::Null,
        }
    }
    fn check_operation(&self, operation: &Operation) -> Result<(), MonitorError> {
        if self
            .operation()
            .is_none_or(|current| !current.same_identity(operation))
        {
            return Err(MonitorError::Refused("Monitor operation identity changed"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    OutputEvent {
        id: String,
        payload: crate::monitor_matches::OutputEvent,
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
    pub record: studio_runtime_store::MonitorRecord,
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
    let f = &snapshot.record;
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
    if running && f.cancel_requested == Some(true) {
        return Ok(decision);
    }
    if running {
        decision.record.cancel_requested = Some(true);
        decision.effects.push(Effect::Terminate);
    } else {
        decision.record.status = MonitorStatus::Cancelled;
        decision.record.finished = Some(monitor_time(now)?);
        decision.record.configuration_pending = Some(false);
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
    if snapshot.record.status != MonitorStatus::Running {
        return Ok(decision);
    }
    if chunk.len() > CHUNK_BYTES {
        return Err(MonitorError::Refused("Monitor output chunk exceeds bound"));
    }
    let bytes = (snapshot.record.bytes.get() as u64)
        .checked_add(chunk.len() as u64)
        .ok_or(MonitorError::Refused("Monitor output counter exhausted"))?;
    crate::monitor_matches::append(&mut decision.record, stream, chunk, snapshot.active_owner())?;
    output_effect(snapshot, &mut decision, now, false);
    decision.record.bytes = monitor_integer(i64::try_from(bytes).map_err(|_| MonitorError::Refused("Monitor output counter exhausted"))?)?;
    if !chunk.is_empty() {
        decision.record.activity_at = monitor_time(now)?;
        decision.record.activity_generation = monitor_integer(snapshot.record.activity_generation.get().checked_add(1).ok_or(MonitorError::Refused("Monitor activity counter exhausted"))?)?;
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
    if match (snapshot.operation(), operation) {
        (Some(current), Some(expected)) => !current.same_identity(expected),
        (None, None) => false,
        _ => true,
    } {
        return Err(MonitorError::Refused(
            "Monitor result belongs to a different command operation",
        ));
    }
    if code.is_none() && error.is_none_or(str::is_empty) {
        return Err(MonitorError::Refused(
            "Monitor result has no definitive outcome",
        ));
    }
    if operation.is_none() && snapshot.record.status == MonitorStatus::Running {
        return Err(MonitorError::Refused(
            "A submitted command requires a native operation identity",
        ));
    }
    let mut decision = snapshot.decision();
    // The real answer of a command lost at its deadline completes it once.
    let late = snapshot.deadline_lost();
    if !snapshot.record.status.terminal() || late {
        decision.record.reattach_recovery = None;
        crate::monitor_matches::flush(&mut decision.record)?;
        output_effect(snapshot, &mut decision, now, true);
        decision.record.status = if snapshot.record.cancel_requested == Some(true) {
            MonitorStatus::Cancelled
        } else if error.is_some_and(|e| !e.is_empty())
            || !code.is_some_and(|c| snapshot.record.success_exit_codes.values().iter().any(|value| value.get() == c))
        {
            MonitorStatus::Failed
        } else {
            MonitorStatus::Completed
        };
        decision.record.exit_code = code;
        decision.record.error = error.map(monitor_text).transpose()?;
        decision.record.finished = Some(monitor_time(now)?);
        decision.record.configuration_pending = Some(false);
        if snapshot.record.cancel_requested == Some(true) && snapshot.record().cancel_dispatch.is_some() {
            decision.record.cancel_dispatch = Some(studio_runtime_schema::monitor_record::MonitorCancelDispatch { status: studio_runtime_schema::monitor_record::MonitorCancelStatus::Done, attempts: None, updated_at: Some(now), next_attempt_at: None, error: None });
        }
        if late {
            decision.record.late_answer_at = Some(now);
        }
    }
    decision.effects.push(
        if snapshot
            .record
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
    if snapshot.record.rule_id.is_none()
        && snapshot.active_owner()
        && let Some((id, payload)) =
            crate::monitor_matches::pending(&mut decision.record, now, final_chunk)
    {
        decision.effects.push(Effect::OutputEvent { id, payload });
    }
}

pub fn tick(snapshot: &MonitorSnapshot, now: f64) -> Result<Decision, MonitorError> {
    let f = &snapshot.record;
    let mut decision = snapshot.decision();
    if snapshot.recovering() {
        return Ok(decision);
    }
    if f.status == MonitorStatus::Running {
        output_effect(snapshot, &mut decision, now, false);
    }
    let quiet = (now - f.activity_at.seconds()).max(0.0) as u64;
    // A cancelled command waits only for its exit receipt; a stall notice would
    // wake an owner that already asked it to stop.
    if f.status != MonitorStatus::Running
        || f.cancel_requested == Some(true)
        || !snapshot.active_owner()
        || f.stall_timeout_seconds.get() == 0
        || quiet < f.stall_timeout_seconds.get() as u64
        || f.activity_generation.get() <= f.stall_wake_generation.get()
    {
        return Ok(decision);
    }
    if f.stall_probe.0.as_ref().is_some_and(|lease| {
        lease.stall_probe_generation.get() == f.activity_generation.get() && now - lease.stall_probe_started.seconds() < PROBE_LEASE_SECONDS
    }) {
        return Ok(decision);
    }
    decision.record.last_stall_at = Some(now);
    if f.liveness_command.as_str().is_empty() {
        decision.record.stall_wake_generation = monitor_integer(f.activity_generation.get())?;
        decision.record.stall_probe.0 = None;
        decision.effects.push(Effect::StallEvent {
            generation: f.activity_generation.get(),
            payload: stall_payload(snapshot.id(), quiet),
        });
    } else {
        decision.record.stall_probe.0 = Some(studio_runtime_schema::monitor_record::MonitorProbeLease { stall_probe_generation: monitor_integer(f.activity_generation.get())?, stall_probe_started: monitor_time(now)? });
        decision.effects.push(Effect::Probe {
            generation: f.activity_generation.get(),
            command: f.liveness_command.as_str().to_owned(),
        });
    }
    Ok(decision)
}

pub fn probe_result(
    snapshot: &MonitorSnapshot,
    generation: i64,
    result: studio_runtime_schema::monitor_record::MonitorProbeResult,
    now: f64,
) -> Result<Decision, MonitorError> {
    let mut decision = snapshot.decision();
    let f = &snapshot.record;
    if f.status != MonitorStatus::Running
        || snapshot.actor.deleted_at.is_some_and(|at| at != 0.0)
        || snapshot.actor.epoch != f.epoch.get()
        || f.stall_probe.0.as_ref().map(|lease| lease.stall_probe_generation.get()) != Some(generation)
        || f.activity_generation.get() != generation
    {
        return Ok(decision);
    }
    let quiet = (now - f.activity_at.seconds()).max(0.0) as u64;
    let mut payload = stall_payload(snapshot.id(), quiet);
    payload["livenessResult"] = json!(result);
    decision.record.stall_wake_generation = monitor_integer(generation)?;
    decision.record.last_liveness_result = Some(result);
    decision.record.stall_probe.0 = None;
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

/// Native input effect identity, including the complete caller ticket.
pub fn input_effect_identity(monitor: &str, ticket: &str) -> String {
    format!("monitor-input:{monitor}:{ticket}")
}

pub fn input_effect_ticket<'a>(monitor: &str, identity: &'a str) -> Option<&'a str> {
    identity
        .strip_prefix(&input_effect_identity(monitor, ""))
        .filter(|ticket| !ticket.is_empty())
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
    let f = &snapshot.record;
    if f.status != MonitorStatus::Running
        || !snapshot.active_owner()
        || f.cancel_requested == Some(true)
        || !f.interactive
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
    if snapshot.record.last_input_attempt.as_ref().is_some_and(|attempt| attempt.id == attempt_id)
    {
        return Err(MonitorError::Refused(
            "Monitor input ticket already settled",
        ));
    }
    let resize = input.rows.is_some() || input.cols.is_some();
    let (method, params, input_method) = if resize {
        (
            "command/exec/resize",
            json!({"processId": f.id, "size": {"rows": input.rows.unwrap_or(24), "cols": input.cols.unwrap_or(80)}}),
            studio_runtime_schema::monitor_record::MonitorInputMethod::Resize(studio_runtime_schema::monitor_record::MonitorResizeParams { process_id: f.id.clone(), size: studio_runtime_schema::monitor_record::MonitorSize { rows: input.rows.unwrap_or(24), cols: input.cols.unwrap_or(80) } }),
        )
    } else {
        if f.stdin_closed == Some(true) || f.stdin_close_requested.is_some() {
            return Err(MonitorError::Refused(
                "This monitor stdin is closed or its close acknowledgement is pending",
            ));
        }
        (
            "command/exec/write",
            json!({"processId": f.id, "deltaBase64": base64::engine::general_purpose::STANDARD.encode(input.text.as_bytes()), "closeStdin": input.close_stdin}),
            studio_runtime_schema::monitor_record::MonitorInputMethod::Write(studio_runtime_schema::monitor_record::MonitorWriteParams { process_id: f.id.clone(), delta_base64: base64::engine::general_purpose::STANDARD.encode(input.text.as_bytes()), close_stdin: input.close_stdin }),
        )
    };
    let mut decision = snapshot.decision();
    decision.record.input_attempt = Some(studio_runtime_schema::monitor_record::MonitorInputAttempt {
        id: attempt_id.to_owned(), operation: operation.clone(), method: input_method,
        status: studio_runtime_schema::monitor_record::MonitorInputStatus::Submitted,
        created: now, finished: None, error: None, response: None,
    });
    if !resize && input.close_stdin {
        decision.record.stdin_close_requested = Some(attempt_id.to_owned());
        decision.record.stdin_error = None;
    }
    decision.effects.push(Effect::Native {
        identity: input_effect_identity(&f.id, attempt_id),
        method,
        params,
    });
    Ok(decision)
}

#[derive(Clone)]
pub enum InputOutcome {
    Expired,
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
        || snapshot.record.status != MonitorStatus::Running
        || snapshot.record.cancel_requested == Some(true)
    {
        return Ok(decision);
    }
    let Some(attempt) = &snapshot.record.input_attempt else {
        return Ok(decision);
    };
    if attempt.id != attempt_id
        || attempt.operation.agent != operation.agent
        || attempt.operation.epoch != operation.epoch
        || attempt.operation.account_key != operation.account_key
        || attempt.operation.connection_id != operation.connection_id
    {
        return Err(MonitorError::Refused(
            "Monitor input receipt identity changed",
        ));
    }
    let mut receipt = attempt.clone();
    receipt.finished = Some(now);
    receipt.status = match outcome {
        InputOutcome::Acknowledged => studio_runtime_schema::monitor_record::MonitorInputStatus::Acknowledged,
        InputOutcome::Rejected(_) | InputOutcome::Expired => studio_runtime_schema::monitor_record::MonitorInputStatus::Failed,
        InputOutcome::Unknown(_) => studio_runtime_schema::monitor_record::MonitorInputStatus::Uncertain,
    };
    let error = match &outcome {
        InputOutcome::Acknowledged => None,
        InputOutcome::Expired => Some("Request id expired"),
        InputOutcome::Rejected(error) | InputOutcome::Unknown(error) => Some(error.as_str()),
    };
    if let Some(error) = error {
        if error.chars().count() > 12000 { return Err(MonitorError::Refused("Monitor input error exceeds bound")); }
        receipt.error = Some(error.to_owned());
    }
    if matches!(outcome, InputOutcome::Unknown(_)) {
        decision.record.input_attempt = Some(receipt);
    } else {
        decision.record.input_attempt = None;
        decision.record.last_input_attempt = Some(receipt);
    }
    if snapshot.record.stdin_close_requested.as_deref() == Some(attempt_id) {
        if !matches!(outcome, InputOutcome::Unknown(_)) { decision.record.stdin_close_requested = None; }
        if let Some(error) = error { decision.record.stdin_error = Some(error.to_owned()); }
        if matches!(outcome, InputOutcome::Acknowledged) {
            decision.record.stdin_closed = Some(true);
            decision.record.stdin_error = None;
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
    if snapshot.record.status != MonitorStatus::Starting
        || !snapshot.active_owner()
        || operation.agent.as_str() != snapshot.agent()
        || operation.epoch.get() != snapshot.actor.epoch
        || operation.account_key.as_str() != snapshot.actor.account_key
    {
        return Err(MonitorError::Refused("Monitor start ownership changed"));
    }
    if params.get("processId").and_then(Value::as_str) != Some(snapshot.id())
        || params.get("cwd").and_then(Value::as_str) != Some(snapshot.record.cwd.as_str())
        || params.get("timeoutMs").and_then(Value::as_i64) != Some(snapshot.record.timeout_ms.get())
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
    decision.record.operation = Some(operation.clone());
    decision.record.status = MonitorStatus::Running;
    decision.record.error = None;
    decision.record.configuration_pending = Some(false);
    decision.record.activity_at = monitor_time(now)?;
    decision.record.activity_generation = monitor_integer(0)?;
    decision.record.stall_wake_generation = monitor_integer(-1)?;
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

/// Proof of a loss decided because the provider gave no answer by the command
/// deadline: `timeout_ms` after creation plus the provider grace. The provider
/// process may still answer; see [`MONITOR_LATE_ANSWER_SECONDS`].
pub const DEADLINE_PASSED: &str = "deadline_passed";

/// How long after a deadline loss the command's real answer still completes it.
/// Owner: monitor deadline policy. Unit: seconds.
pub const MONITOR_LATE_ANSWER_SECONDS: f64 = 3600.0;

const DEADLINE_LOST_ERROR: &str =
    "Command deadline passed without an answer. Outcome unknown; not rerun.";

/// A supervisor's operation receipt is evidence of the original command; it is never
/// permission to run it again. The connection owner supplies this verified receipt.
pub struct ReattachProof {
    pub operation: Operation,
    pub operation_id: String,
    /// The provider's request id when its operation status proved the command.
    /// `None` when the proof is Studio's retained request, which the provider
    /// process of the same generation still owes an answer.
    pub native_id: Option<u64>,
    pub current_connection: String,
}

/// A retained-process observation never launches a command.
pub enum ReattachOutcome {
    Pending,
    Running(Box<ReattachProof>),
    TerminalQueued,
    Gone(String),
}

/// Keep commands active while the new backend reconciles their retained process.
/// No completion is published until a native result or a failed reattach decides it.
pub fn restart_recovery(snapshot: &MonitorSnapshot, now: f64) -> Result<Decision, MonitorError> {
    if !matches!(
        snapshot.record.status,
        MonitorStatus::Running | MonitorStatus::Starting | MonitorStatus::Approval
    ) {
        return Err(MonitorError::Refused("Monitor is not active"));
    }
    let mut decision = snapshot.decision();
    if snapshot.recovering() {
        return Ok(decision);
    }
    decision.record.reattach_recovery = Some(ReattachRecovery {
        account_key: snapshot.command_account().to_owned(),
        epoch: snapshot.actor.epoch,
        state: ReattachState::Running { started_at: now },
        proof: None,
        error: None,
        finished: None,
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
        .record
        .reattach_recovery
        .clone()
        .filter(|recovery| matches!(recovery.state, ReattachState::Running { .. }))
    else {
        return Ok(decision);
    };
    let ReattachState::Running { started_at } = recovery.state else {
        return Ok(decision);
    };
    recovery.state = ReattachState::NotReattachable {
        started_at: Some(started_at),
    };
    recovery.proof = Some(reason.into());
    decision.record.reattach_recovery = Some(recovery);
    decision.record.status = MonitorStatus::Lost;
    decision.record.finished = Some(monitor_time(now)?);
    decision.record.error = Some(monitor_text(if reason == DEADLINE_PASSED {
        DEADLINE_LOST_ERROR
    } else {
        "Command outcome unknown after losing the provider result; not rerun."
    })?);
    decision.effects.push(if snapshot.record.rule_id.is_some() {
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
    let recovery = snapshot.record.reattach_recovery.as_ref();
    if !matches!(
        snapshot.record.status,
        MonitorStatus::Running | MonitorStatus::Starting | MonitorStatus::Approval
    ) || proof.operation_id != format!("monitor:{}", snapshot.id())
        || proof.current_connection.is_empty()
        || proof.operation.account_key.as_str() != snapshot.command_account()
        || recovery.is_none_or(|recovery| {
            !matches!(recovery.state, ReattachState::Running { .. })
                || recovery.epoch != snapshot.actor.epoch
                || recovery.account_key != snapshot.command_account()
        })
    {
        return Err(MonitorError::Refused(
            "Monitor reattach proof does not match its command",
        ));
    }
    let mut decision = snapshot.decision();
    decision.record.status = MonitorStatus::Running;
    decision.record.reattached_connection_id = Some(proof.current_connection.clone());
    decision.record.reattach_recovery = None;
    if !snapshot.active_owner() {
        // A verified survivor cannot resume an inactive owner. Latch the ordinary
        // cancellation path so deletion or an epoch change cannot orphan it.
        decision.record.cancel_requested = Some(true);
        decision.effects.push(Effect::Terminate);
    }
    Ok(decision)
}

pub enum ReattachDisposition {
    AwaitExit,
    Cancel,
    /// Another transaction settled or reattached this exact command.
    Superseded,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MonitorCreateInput {
    pub command: String,
    #[serde(default, deserialize_with = "present_input")]
    pub timeout_ms: Option<i64>,
    #[serde(default, deserialize_with = "present_input")]
    pub stall_timeout_seconds: Option<i64>,
    #[serde(default, deserialize_with = "present_input")]
    pub liveness_command: Option<String>,
    #[serde(default, deserialize_with = "present_input")]
    pub wake_on: Option<studio_runtime_schema::monitor_record::MonitorWakeOn>,
    #[serde(default, deserialize_with = "present_input")]
    pub success_exit_codes: Option<Vec<i64>>,
    #[serde(default, deserialize_with = "present_input")]
    pub interactive: Option<bool>,
    pub output_patterns: Option<Vec<String>>,
}
fn present_input<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MonitorCreateOwner {
    id: String, epoch: i64, cwd: String, turn_id: Option<String>,
    yolo_mode: Option<bool>, approval_policy: Option<String>,
}
/// Build the initial persisted row from checked owner and command input.
pub fn create_record(
    actor: &Value, input: &MonitorCreateInput, request_id: &str, log_root: &str, now: f64,
) -> Result<studio_runtime_store::MonitorRecord, MonitorError> {
    use studio_runtime_schema::monitor_record::{MonitorExitCodes, MonitorProbeState, MonitorWakeOn};
    let owner = MonitorCreateOwner::deserialize(actor).map_err(|_| MonitorError::Refused("Unknown agent"))?;
    if input.command.trim().is_empty() { return Err(MonitorError::Refused("Supply a command")); }
    let output_patterns = crate::monitor_matches::patterns(input.output_patterns.as_deref())?;
    let approved = owner.yolo_mode.unwrap_or(owner.approval_policy.as_deref() == Some("never"));
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, request_id.as_bytes()).to_string();
    let record = studio_runtime_store::MonitorRecord {
        id: id.clone(), agent: owner.id, epoch: monitor_integer(owner.epoch)?, turn_id: owner.turn_id,
        request_id: request_id.to_owned(), command: monitor_text(&input.command)?, cwd: owner.cwd,
        timeout_ms: monitor_integer(input.timeout_ms.unwrap_or(3_600_000))?,
        stall_timeout_seconds: monitor_integer(input.stall_timeout_seconds.unwrap_or(1800))?,
        liveness_command: monitor_text(input.liveness_command.as_deref().unwrap_or("").trim())?,
        activity_at: monitor_time(now)?, activity_generation: monitor_integer(0)?, stall_wake_generation: monitor_integer(-1)?,
        wake_on: input.wake_on.unwrap_or(MonitorWakeOn::Exit),
        success_exit_codes: MonitorExitCodes::try_from(input.success_exit_codes.clone().unwrap_or_else(|| vec![0])).map_err(|_| MonitorError::Refused("Invalid monitor exit codes"))?,
        status: if approved { MonitorStatus::Starting } else { MonitorStatus::Approval }, created: monitor_time(now)?,
        exit_code: None, tail: monitor_text("")?, bytes: monitor_integer(0)?, interactive: input.interactive.unwrap_or(false),
        rule_id: None, log: format!("{log_root}/{id}.log"), output_patterns: Some(output_patterns), output_truncated: None,
        output_event_sequence: Some(0), output_event_count: Some(0), output_event_ids: Some(vec![]),
        cancel_requested: None, configuration_pending: None, error: None, finished: None, input_attempt: None,
        last_input_attempt: None, reattach_recovery: None, stream_queue: None, stream_ack_cursor: None,
        account_key: None, cancel_dispatch: None, cancel_provider_request: None, cancel_root_request_id: None,
        root_request_id: None, reattached_connection_id: None, last_stall_at: None, late_answer_at: None,
        liveness_probe: None, last_liveness_result: None, stdin_error: None, restart_after: None, restart_signature: None,
        shell_pressure_cleanup: None, delivery_failure: None, task_gate: None, output_line_offsets: None,
        output_line_tail: None, output_match_excerpts: None, output_matcher_tail: None, output_matches_pending: None,
        output_matches_total: None, output_utf8_pending: None, output_wake_at: None, output_wake_budget: None,
        last_exit_code: None, operation: None, stall_probe: MonitorProbeState(None), stdin_close_requested: None, stdin_closed: None,
    };
    MonitorSnapshot::checked_stored(&record, actor.clone())?;
    Ok(record)
}

pub fn same_request(previous: &studio_runtime_store::MonitorRecord, proposed: &studio_runtime_store::MonitorRecord) -> bool {
    previous.agent == proposed.agent && previous.command == proposed.command && previous.timeout_ms == proposed.timeout_ms
        && previous.interactive == proposed.interactive && previous.wake_on == proposed.wake_on
        && previous.success_exit_codes == proposed.success_exit_codes && previous.stall_timeout_seconds == proposed.stall_timeout_seconds
        && previous.liveness_command == proposed.liveness_command
        && previous.output_patterns.as_deref().unwrap_or_default() == proposed.output_patterns.as_deref().unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn complete_test_monitor(record: Value) -> Value {
    let mut complete = json!({"id":"watch","agent":"agent","epoch":0,"status":"running","requestId":"fixture","created":0,"activityAt":0,"activityGeneration":0,"bytes":0,"command":"","cwd":std::env::temp_dir(),"interactive":false,"livenessCommand":"","log":std::env::temp_dir().join("watch.log"),"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit"});
    complete.as_object_mut().unwrap().extend(record.as_object().unwrap().clone());
    complete
}
