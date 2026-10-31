//! Rule decisions on checked persisted records. No clock, filesystem, store or provider I/O.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::event_write::{python_strip, truthy};

/// Maximum automatic catch-up delay for one-shot rules, in seconds.
pub const ONCE_RULE_GRACE_SECONDS: f64 = 60.0;

pub const OUTPUT_CHARACTERS: usize = 12_000;
pub const DEFAULT_RULE_STALL_SECONDS: u64 = 1800;
/// Delay past the exact low-worker duration because its alert predicate is strict `>`.
pub const LOW_WORKER_STRICT_DEADLINE_MARGIN_SECONDS: f64 = 0.001;
const MIN_COMMAND_MILLISECONDS: u64 = 1000;
const MAX_COMMAND_MILLISECONDS: u64 = 86_400_000;
const STOPPED: &str = "The agent was stopped. Resume this rule explicitly.";
const RESTARTED: &str = "Server restarted during a check. Outcome unknown; check was not repeated.";
use studio_runtime_schema::rule_record::OwnerHold;
fn owner_hold_reason(hold: OwnerHold) -> &'static str {
    match hold {
        OwnerHold::FailedInputHeld => {
            "Paused: the agent failed and its input waits, so it cannot act on a wake. The rule resumes when the agent can run again."
        }
        OwnerHold::NativeFailureHold => {
            "Paused: the agent is held after a failure, so it cannot act on a wake. The rule resumes when the agent can run again."
        }
        OwnerHold::Archived => {
            "Paused: the conversation of the agent is archived. The rule resumes when the conversation is restored."
        }
    }
}
const OWNER_FIELDS: &[&str] = &[
    "epoch",
    "status",
    "archived",
    "accountKey",
    "threadId",
    "autoWake",
    "deletedAt",
    "restartRecovery",
    "disconnectRecovery",
    "nativeFailureHold",
    "approvalPolicy",
    "sandbox",
    "profile",
    "role",
];

#[derive(Debug, Error)]
pub enum RuleError {
    #[error("invalid stored object {object} at {location}")]
    Invalid { object: String, location: String },
    #[error("invalid rule observation: {0}")]
    Observation(&'static str),
    #[error("rule schema unavailable")]
    Schema,
}

use studio_runtime_schema::rule_record::{RuleKind as Kind, RuleStatus as Status};

fn can_tick(rule: &studio_runtime_store::RuleRecord) -> bool {
    rule.status == Status::Active && !rule.in_flight
}
fn owner_paused(rule: &studio_runtime_store::RuleRecord) -> bool {
    rule.status == Status::Paused && !rule.in_flight && rule.owner_hold.is_some()
}
/// Whether a tick can change this stored rule.
pub fn tick_can_change(rule: &studio_runtime_store::RuleRecord) -> bool {
    can_tick(rule) || owner_paused(rule)
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recovery {
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    auto_wake: bool,
    #[serde(default)]
    epoch: Option<u64>,
    #[serde(default)]
    account_key: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Owner {
    id: String,
    epoch: u64,
    status: String,
    auto_wake: bool,
    #[serde(default)]
    deleted_at: Option<f64>,
    #[serde(default)]
    account_key: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    restart_recovery: Option<Recovery>,
    #[serde(default)]
    disconnect_recovery: Option<Recovery>,
    #[serde(default)]
    native_failure_hold: Value,
    #[serde(default)]
    archived: Value,
}
impl Owner {
    fn deleted(&self) -> bool {
        self.deleted_at.is_some_and(|value| value != 0.0)
    }
    /// Why the owner cannot act on a wake now, while it can run again later.
    /// `held_input` says whether a failed start kept its input pending.
    fn hold(&self, held_input: bool) -> Option<OwnerHold> {
        if truthy(Some(&self.archived)) {
            Some(OwnerHold::Archived)
        } else if truthy(Some(&self.native_failure_hold)) {
            Some(OwnerHold::NativeFailureHold)
        } else if self.status == "failed" && held_input {
            Some(OwnerHold::FailedInputHeld)
        } else {
            None
        }
    }
    fn matching_recovery(&self, receipt: &Recovery, epoch: u64) -> bool {
        receipt.auto_wake
            && receipt.epoch == Some(epoch)
            && self.epoch == epoch
            && receipt.account_key == self.account_key
            && receipt.thread_id == self.thread_id
    }
    fn restart_pending(&self) -> bool {
        self.restart_recovery.as_ref().is_some_and(|receipt| {
            receipt.stage.as_deref() == Some("pending")
                && self.matching_recovery(receipt, self.epoch)
        })
    }
    fn recovery_pending(&self, epoch: u64) -> bool {
        (self.epoch == epoch && self.restart_pending())
            || ((!self.auto_wake || truthy(Some(&self.native_failure_hold)))
                && self
                    .disconnect_recovery
                    .as_ref()
                    .is_some_and(|receipt| self.matching_recovery(receipt, epoch)))
    }
    fn finish_allowed(&self) -> bool {
        self.auto_wake || (!self.deleted() && self.status != "paused" && self.restart_pending())
    }
}

/// A single checked constructor is shared by ticks, callbacks, recovery and corpus tests.
#[derive(Clone)]
pub struct RuleSnapshot {
    raw: studio_runtime_store::RuleRecord,
    owner_raw: Value,
    owner: Owner,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleCheck {
    pub rule_id: String,
    pub check_number: u64,
    pub agent_id: String,
    pub epoch: u64,
    pub account_key: Option<String>,
    pub thread_id: Option<String>,
    pub monitor_id: String,
    pub command: String,
    pub timeout_ms: u64,
    pub liveness: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuleWake {
    pub id: String,
    pub kind: &'static str,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuleDecision {
    pub record: Option<studio_runtime_store::RuleRecord>,
    pub wake: Option<RuleWake>,
    pub check: Option<RuleCheck>,
}

#[derive(Clone, Copy)]
pub struct CheckResult<'a> {
    pub code: Option<i64>,
    pub error: Option<&'a str>,
    /// Whether the monitor reached its cancelled terminal state.
    pub cancelled: bool,
    pub output: &'a str,
}

/// File metadata is supplied only when requested, including `None` for a missing file.
#[derive(Clone, Copy, Debug, Default)]
pub struct TickObservation {
    pub fingerprint: Option<Option<(i64, u64, u64)>>,
    pub active_workers: Option<u64>,
    /// Whether the owner has input that a failed start kept pending at its
    /// epoch. Read only when [`RuleSnapshot::held_input_owner`] asks for it.
    pub held_input: bool,
}

impl RuleSnapshot {
    #[cfg(test)]
    pub fn from_records(rule: &Value, owner: Value) -> Result<Self, RuleError> {
        let record: studio_runtime_store::RuleRecord = serde_path_to_error::deserialize(rule)
            .map_err(|error| invalid("rules", rule, &format!("/{}", error.path())))?;
        Self::from_stored(&record, owner)
    }
    pub fn from_stored(
        record: &studio_runtime_store::RuleRecord,
        owner: Value,
    ) -> Result<Self, RuleError> {
        let agent: Owner = checked("agents", "PersistedAgentRecord", &owner)?;
        if record.agent != agent.id {
            return Err(RuleError::Invalid {
                object: format!("rules/{}", record.id),
                location: "/agent".into(),
            });
        }
        Ok(Self {
            raw: record.clone(),
            owner_raw: owner,
            owner: agent,
        })
    }

    pub fn id(&self) -> &str {
        &self.raw.id
    }
    pub(crate) fn launch_intent(&self) -> Result<Option<RuleCheck>, RuleError> {
        let Some(intent) = &self.raw.launch_intent else {
            return Ok(None);
        };
        Ok(Some(RuleCheck {
            rule_id: intent.rule_id.clone(),
            check_number: intent.check_number.get() as u64,
            agent_id: intent.agent_id.clone(),
            epoch: intent.epoch.get() as u64,
            account_key: intent.account_key.clone(),
            thread_id: intent.thread_id.clone(),
            monitor_id: intent.monitor_id.clone(),
            command: intent.command.as_str().to_owned(),
            timeout_ms: intent.timeout_ms.get() as u64,
            liveness: intent.liveness,
        }))
    }
    pub fn owner_id(&self) -> &str {
        &self.raw.agent
    }
    pub fn record(&self) -> &studio_runtime_store::RuleRecord {
        &self.raw
    }
    pub fn is_low_workers(&self) -> bool {
        matches!(self.raw.kind, Kind::LowWorkers { .. })
    }
    /// The owner id and epoch whose held input a tick must observe: only a
    /// failed owner can hold its input, and only a rule that can change.
    pub fn held_input_owner(&self) -> Option<(&str, u64)> {
        (self.owner.status == "failed" && (can_tick(&self.raw) || owner_paused(&self.raw)))
            .then_some((self.owner.id.as_str(), self.owner.epoch))
    }
    pub fn current(&self, rule: &studio_runtime_store::RuleRecord, owner: &Value) -> bool {
        rule == &self.raw
            && OWNER_FIELDS
                .iter()
                .all(|field| owner.get(field) == self.owner_raw.get(field))
    }
    fn stalled(&self, now: f64) -> bool {
        let r = &self.raw;
        matches!(r.kind, Kind::File { .. })
            && r.stall_timeout_seconds.get() != 0
            && now
                - r.file_activity_at
                    .as_ref()
                    .map(seconds)
                    .unwrap_or_else(|| seconds(&r.created))
                >= r.stall_timeout_seconds.get() as f64
            && r.file_generation.get() > r.stall_wake_generation.get()
    }
    pub fn fingerprint_path(&self, now: f64) -> Option<&str> {
        (matches!(self.raw.kind, Kind::File { .. })
            && self.raw.status == Status::Active
            && !self.raw.in_flight
            && !self.owner.deleted()
            && self.owner.auto_wake
            && self.owner.epoch == self.raw.epoch.get() as u64
            && !self.owner.recovery_pending(self.raw.epoch.get() as u64)
            && (self.stalled(now) || seconds(&self.raw.next_at) <= now))
            .then_some(match &self.raw.kind {
                Kind::File { path } => Some(path.as_str()),
                Kind::Interval | Kind::Once | Kind::Event | Kind::LowWorkers { .. } => None,
            })
            .flatten()
    }
    fn with_record(&self, record: &studio_runtime_store::RuleRecord) -> Result<Self, RuleError> {
        Self::from_stored(record, self.owner_raw.clone())
    }
    fn change(&self, record: studio_runtime_store::RuleRecord) -> RuleDecision {
        RuleDecision {
            record: Some(record),
            ..RuleDecision::default()
        }
    }

    /// Rechecked by the effect adapter before submitting a durable launch intent.
    pub fn launch_allowed(&self, check: &RuleCheck) -> bool {
        self.raw.status == Status::Active
            && self.raw.in_flight
            && self.owner.auto_wake
            && !self.owner.deleted()
            && self.owner.epoch == check.epoch
            && !self.owner.recovery_pending(check.epoch)
            && &self.check() == check
    }

    pub fn check(&self) -> RuleCheck {
        let r = &self.raw;
        RuleCheck {
            rule_id: r.id.clone(),
            check_number: r.checks.get() as u64,
            agent_id: r.agent.clone(),
            epoch: r.epoch.get() as u64,
            account_key: self.owner.account_key.clone(),
            thread_id: self.owner.thread_id.clone(),
            monitor_id: Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("rule:{}:{}", r.id, r.checks.get() as u64).as_bytes(),
            )
            .to_string(),
            command: if r.stall_probe == Some(true) {
                &r.liveness_command
            } else {
                &r.command
            }
            .as_str()
            .to_owned(),
            timeout_ms: (r.interval_seconds.get() as u64)
                .saturating_mul(1000)
                .clamp(MIN_COMMAND_MILLISECONDS, MAX_COMMAND_MILLISECONDS),
            liveness: r.stall_probe.unwrap_or(false),
        }
    }

    pub fn tick(&self, now: f64, observation: TickObservation) -> Result<RuleDecision, RuleError> {
        finite_time(now)?;
        let r = &self.raw;
        if owner_paused(r) {
            return Ok(self.resume_after_owner_hold(observation.held_input));
        }
        if !can_tick(r) {
            return Ok(RuleDecision::default());
        }
        if !self.owner.deleted()
            && self.owner.epoch == r.epoch.get() as u64
            && self.owner.recovery_pending(r.epoch.get() as u64)
        {
            return Ok(RuleDecision::default());
        }
        let mut next = self.raw.clone();
        if self.owner.deleted() || !self.owner.auto_wake || self.owner.epoch != r.epoch.get() as u64
        {
            next.status = Status::Paused;
            next.error = Some(STOPPED.into());
            return Ok(self.change(next));
        }
        // An owner that cannot act on a wake gets no new check or wake; each
        // one would only start a command and leave its result unread.
        if let Some(hold) = self.owner.hold(observation.held_input) {
            next.status = Status::Paused;
            next.owner_hold = Some(hold);
            next.error = Some(owner_hold_reason(hold).into());
            return Ok(self.change(next));
        }
        if let Kind::LowWorkers {
            minimum_workers,
            duration_minutes,
        } = &r.kind
        {
            return self.low_workers(
                now,
                observation.active_workers,
                minimum_workers.get() as u64,
                duration_minutes.get() as u64,
            );
        }
        if matches!(r.kind, Kind::Event) {
            return Ok(RuleDecision::default());
        }
        if self.stalled(now) {
            let fingerprint = observation
                .fingerprint
                .ok_or(RuleError::Observation("file fingerprint missing"))?;
            if fingerprint != r.fingerprint {
                next.fingerprint = fingerprint;
                next.file_activity_at = Some(time_number(now)?);
                next.file_generation = rule_integer(r.file_generation.get() as u64 + 1)?;
                return Ok(self.change(next));
            }
            let text = json!({"rule":r.id,"name":r.name,"message":"file unchanged",
                "path":match &r.kind { Kind::File { path } => Some(path.as_str()), Kind::Interval | Kind::Once | Kind::Event | Kind::LowWorkers { .. } => None },"quietSeconds":(now-r.file_activity_at.as_ref().map(seconds).unwrap_or(now)) as i64})
            .to_string();
            let id = format!("rule-stall:{}:{}", r.id, r.file_generation.get() as u64);
            if !r.liveness_command.as_str().is_empty() {
                next.in_flight = true;
                next.checks = rule_integer(r.checks.get() as u64 + 1)?;
                next.stall_probe = Some(true);
                next.stall_event_key = Some(id);
                next.stall_text = Some(text);
                return self.launch(&next, now);
            }
            next.stall_wake_generation = rule_integer(r.file_generation.get() as u64)?;
            return Ok(RuleDecision {
                record: Some(next),
                wake: Some(RuleWake {
                    id,
                    kind: "rule_stall",
                    text,
                }),
                check: None,
            });
        }
        if seconds(&r.next_at) > now {
            return Ok(RuleDecision::default());
        }
        if matches!(r.kind, Kind::Once) && now - seconds(&r.next_at) > ONCE_RULE_GRACE_SECONDS {
            let message = "Missed scheduled run. Resume this rule to schedule a new run, or delete it to cancel.";
            next.status = Status::Paused;
            next.error = Some(message.into());
            next.wakes = rule_integer(r.wakes.get() as u64 + 1)?;
            return Ok(RuleDecision {
                record: Some(next),
                wake: Some(RuleWake {
                    id: format!(
                        "rule-missed:{}:{}:{}",
                        r.id,
                        r.epoch.get() as u64,
                        seconds(&r.next_at)
                    ),
                    kind: "rule_missed",
                    text: json!({"rule":r.id,"name":r.name,"dueAt":seconds(&r.next_at),"delaySeconds":now-seconds(&r.next_at),"message":message}).to_string(),
                }),
                check: None,
            });
        }
        next.next_at = time_number(now + r.interval_seconds.get() as u64 as f64)?;
        if matches!(r.kind, Kind::File { .. }) {
            let fingerprint = observation
                .fingerprint
                .ok_or(RuleError::Observation("file fingerprint missing"))?;
            next.fingerprint = fingerprint;
            if fingerprint == r.fingerprint {
                return Ok(self.change(next));
            }
            next.file_activity_at = Some(time_number(now)?);
            next.file_generation = rule_integer(r.file_generation.get() as u64 + 1)?;
        }
        next.in_flight = true;
        next.last_at = Some(time_number(now)?);
        next.checks = rule_integer(r.checks.get() as u64 + 1)?;
        self.launch(&next, now)
    }

    /// Resume a rule that the tick paused for its owner once the owner can
    /// run again. A stopped or deleted owner turns it into an explicit pause.
    fn resume_after_owner_hold(&self, held_input: bool) -> RuleDecision {
        let r = &self.raw;
        let mut next = self.raw.clone();
        let stopped = self.owner.deleted()
            || !self.owner.auto_wake
            || self.owner.epoch != r.epoch.get() as u64;
        if !stopped && let Some(hold) = self.owner.hold(held_input) {
            if r.owner_hold == Some(hold) {
                return RuleDecision::default();
            }
            next.owner_hold = Some(hold);
            next.error = Some(owner_hold_reason(hold).into());
            return self.change(next);
        }
        next.owner_hold = None;
        if stopped {
            next.error = Some(STOPPED.into());
            return self.change(next);
        }
        next.status = Status::Active;
        next.error = None;
        if matches!(r.kind, Kind::LowWorkers { .. }) {
            next.low_since = None;
            next.alerted = Some(false);
        }
        self.change(next)
    }

    // No-command checks finish in the same durable decision; a crash cannot strand their wake.
    fn launch(
        &self,
        next: &studio_runtime_store::RuleRecord,
        now: f64,
    ) -> Result<RuleDecision, RuleError> {
        let snapshot = self.with_record(next)?;
        if snapshot.raw.status != Status::Active
            || !snapshot.owner.auto_wake
            || snapshot.raw.epoch.get() as u64 != snapshot.owner.epoch
        {
            let mut next = snapshot.raw;
            next.in_flight = false;
            return Ok(self.change(next));
        }
        if snapshot.check().command.is_empty() {
            snapshot.finish(
                now,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: snapshot
                        .raw
                        .event_text
                        .as_ref()
                        .map(|text| text.as_str())
                        .unwrap_or_default(),
                },
            )
        } else {
            Ok(RuleDecision {
                check: Some(snapshot.check()),
                ..self.change(snapshot.raw)
            })
        }
    }

    fn low_workers(
        &self,
        now: f64,
        count: Option<u64>,
        minimum: u64,
        duration: u64,
    ) -> Result<RuleDecision, RuleError> {
        let r = &self.raw;
        let count = count.ok_or(RuleError::Observation("worker activity missing"))?;
        let mut next = self.raw.clone();
        next.active_workers = Some(rule_integer(count)?);
        if count >= minimum {
            next.low_since = None;
            next.alerted = Some(false);
        } else {
            let since = r
                .low_since
                .as_ref()
                .map(seconds)
                .filter(|since| now >= *since)
                .unwrap_or(now);
            next.low_since = Some(time_number(since)?);
            let elapsed = now - since;
            if r.alerted != Some(true) && elapsed > (duration * 60) as f64 {
                next.alerted = Some(true);
                next.last_at = Some(time_number(now)?);
                next.checks = rule_integer(r.checks.get() as u64 + 1)?;
                let output = json!({"activeSubagents":count,"minimumSubagents":minimum,
                    "belowThresholdSince":since,"elapsedMinutes":elapsed/60.0})
                .to_string();
                return self.with_record(&next)?.finish(
                    now,
                    CheckResult {
                        code: Some(0),
                        error: None,
                        cancelled: false,
                        output: &output,
                    },
                );
            }
        }
        if next == self.raw {
            Ok(RuleDecision::default())
        } else {
            Ok(self.change(next))
        }
    }

    pub fn event(
        &self,
        kind: &str,
        text: &str,
        key: &str,
        now: f64,
    ) -> Result<RuleDecision, RuleError> {
        finite_time(now)?;
        let alias = match kind {
            "child_completed" | "child_result" => "worker_completed",
            "complaint_submitted" => "complaint",
            _ => kind,
        };
        let r = &self.raw;
        if !matches!(r.kind, Kind::Event)
            || r.event != alias
            || r.status != Status::Active
            || r.in_flight
            || r.epoch.get() as u64 != self.owner.epoch
            || r.last_event.as_deref() == Some(key)
            // The tick pauses this rule; until then an event starts nothing.
            || self.owner.hold(false).is_some()
        {
            return Ok(RuleDecision::default());
        }
        let mut next = self.raw.clone();
        next.in_flight = true;
        next.last_event = Some(key.to_owned());
        next.last_at = Some(time_number(now)?);
        next.checks = rule_integer(r.checks.get() as u64 + 1)?;
        next.event_text = Some(
            text.chars()
                .take(OUTPUT_CHARACTERS)
                .collect::<String>()
                .try_into()
                .map_err(|_| RuleError::Schema)?,
        );
        self.launch(&next, now)
    }

    pub fn finish(&self, now: f64, result: CheckResult<'_>) -> Result<RuleDecision, RuleError> {
        finite_time(now)?;
        let r = &self.raw;
        let mut next = self.raw.clone();
        let output = tail(result.output);
        let mut wake = None;
        let authorized = r.status == Status::Active
            && self.owner.epoch == r.epoch.get() as u64
            && self.owner.finish_allowed();
        if r.stall_probe == Some(true) {
            if authorized && !self.owner.deleted() {
                let id = r
                    .stall_event_key
                    .clone()
                    .ok_or_else(|| RuleError::Invalid {
                        object: format!("rules/{}", self.raw.id),
                        location: "/stallEventKey".into(),
                    })?;
                wake = Some(RuleWake {
                    id,
                    kind: "rule_stall",
                    text: json!({
                    "rule":r.id,"name":r.name,"message":"file unchanged","path":match &r.kind { Kind::File { path } => Some(path.as_str()), Kind::Interval | Kind::Once | Kind::Event | Kind::LowWorkers { .. } => None },
                    "quietSeconds":(now-r.file_activity_at.as_ref().map(seconds).unwrap_or(now)) as i64,
                    "livenessResult":output,"livenessExitCode":result.code,"livenessError":result.error}).to_string(),
                });
            }
            next.in_flight = false;
            next.stall_probe = Some(false);
            next.stall_wake_generation = rule_integer(r.file_generation.get() as u64)?;
            next.last_stall_finished = Some(time_number(now)?);
            next.last_stall_exit_code = result.code;
            next.last_stall_error = result.error.map(str::to_owned);
        } else {
            let suppressed = python_strip(result.output)
                .rsplit([
                    '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}',
                    '\u{2028}', '\u{2029}',
                ])
                .next()
                .and_then(|line| serde_json::from_str::<WakeDirective>(line).ok())
                .is_some_and(|payload| payload.wake_agent == Some(false));
            next.in_flight = false;
            next.last_exit_code = result.code;
            next.error = result.error.map(str::to_owned);
            next.last_output = Some(output.clone().try_into().map_err(|_| RuleError::Schema)?);
            next.last_finished = Some(time_number(now)?);
            if result.code == Some(0)
                && result.error.is_none_or(str::is_empty)
                && !suppressed
                && authorized
            {
                wake = Some(RuleWake {
                    id: format!("rule-wake:{}:{}", r.id, r.checks.get() as u64),
                    kind: "rule",
                    text: json!({"rule":r.id,"name":r.name,"instruction":r.text,"output":output})
                        .to_string(),
                });
                next.wakes = rule_integer(r.wakes.get() as u64 + 1)?;
            }
            if matches!(r.kind, Kind::Once) && r.status == Status::Active {
                next.status = Status::Completed;
            }
        }
        Ok(RuleDecision {
            record: Some(next),
            wake,
            check: None,
        })
    }

    pub fn recover(&self, now: f64) -> Result<RuleDecision, RuleError> {
        finite_time(now)?;
        let r = &self.raw;
        let mut next = self.raw.clone();
        if matches!(r.kind, Kind::File { .. }) && self.raw.file_activity_at.is_none() {
            next.file_activity_at = Some(time_number(now)?);
            next.file_generation = rule_integer(0)?;
            next.stall_wake_generation = (-1).try_into().map_err(|_| RuleError::Schema)?;
            next.stall_timeout_seconds = rule_integer(DEFAULT_RULE_STALL_SECONDS)?;
            next.liveness_command = String::new().try_into().map_err(|_| RuleError::Schema)?;
        }
        if matches!(r.kind, Kind::LowWorkers { .. }) {
            next.low_since = None;
            next.next_at = time_number(now)?;
        }
        if r.in_flight {
            if r.status == Status::Active {
                next.restart_check = Some(studio_runtime_schema::rule_record::RestartCheck {
                    epoch: r.epoch.get() as u64,
                    checks: r.checks.get() as u64,
                    monitor_id: self.check().monitor_id,
                });
            } else {
                next.restart_check = None;
            }
            next.in_flight = false;
            next.status = Status::Paused;
            next.error = Some(RESTARTED.into());
        }
        if next == self.raw {
            Ok(RuleDecision::default())
        } else {
            Ok(self.change(next))
        }
    }

    /// Completion consumes an exact current check, or an exact startup recovery marker, once.
    pub fn finish_check(
        &self,
        check: &RuleCheck,
        result: CheckResult<'_>,
        now: f64,
    ) -> Result<RuleDecision, RuleError> {
        if self.owner.epoch != check.epoch
            || check.rule_id != self.raw.id
            || check.agent_id != self.raw.agent
        {
            return Ok(RuleDecision::default());
        }
        // The reservation records the exact check submitted to the monitor. A rule may be
        // paused while that monitor is still live, so settle that durable intent even when
        // derived settings have since changed. Without a reservation, compare the snapshot.
        let matches_check = match &self.raw.launch_intent {
            Some(_) => self
                .launch_intent()
                .is_ok_and(|intent| intent.as_ref() == Some(check)),
            None => self.check() == *check,
        };
        if !matches_check {
            return Ok(RuleDecision::default());
        }
        if self.raw.in_flight {
            return self.finish(now, result);
        }
        // Python records a canceled monitor's result after the owning rule has
        // already been paused. Keep that receipt while leaving the rule paused.
        if self.raw.status == Status::Paused && result.cancelled {
            return self.finish(now, result);
        }
        let marker = studio_runtime_schema::rule_record::RestartCheck {
            epoch: check.epoch,
            checks: check.check_number,
            monitor_id: check.monitor_id.clone(),
        };
        if self.raw.restart_check.as_ref() != Some(&marker)
            || self.raw.status != Status::Paused
            || self.owner.epoch != check.epoch
            || self.owner.deleted()
        {
            return Ok(RuleDecision::default());
        }
        let mut next = self.raw.clone();
        next.restart_check = None;
        next.status = Status::Active;
        next.error = None;
        self.with_record(&next)?.finish(now, result)
    }
}

pub(crate) fn checked<T: serde::de::DeserializeOwned>(
    object: &str,
    schema: &str,
    value: &Value,
) -> Result<T, RuleError> {
    if let Some((location, _)) =
        studio_runtime_schema::first_error(schema, value).map_err(|_| RuleError::Schema)?
    {
        return Err(invalid(object, value, &location));
    }
    serde_path_to_error::deserialize(value.clone())
        .map_err(|error| invalid(object, value, &format!("/{}", error.path())))
}
fn invalid(object: &str, value: &Value, location: &str) -> RuleError {
    RuleError::Invalid {
        object: format!(
            "{object}/{}",
            value.get("id").and_then(Value::as_str).unwrap_or("unknown")
        ),
        location: location.to_owned(),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WakeDirective {
    wake_agent: Option<bool>,
}
fn seconds(value: &serde_json::Number) -> f64 {
    value.as_f64().expect("JSON number is finite")
}
fn time_number(value: f64) -> Result<serde_json::Number, RuleError> {
    serde_json::Number::from_f64(value).ok_or(RuleError::Observation("non-finite time"))
}
fn rule_integer<const MIN: i64, const MAX: u64>(
    value: u64,
) -> Result<studio_runtime_schema::bounded_integer::BoundedInteger<MIN, MAX>, RuleError> {
    i64::try_from(value)
        .map_err(|_| RuleError::Observation("rule counter exceeded"))?
        .try_into()
        .map_err(|_| RuleError::Observation("rule counter exceeded"))
}
fn finite_time(now: f64) -> Result<(), RuleError> {
    if now.is_finite() {
        Ok(())
    } else {
        Err(RuleError::Observation("non-finite time"))
    }
}
fn tail(text: &str) -> String {
    text.chars()
        .skip(text.chars().count().saturating_sub(OUTPUT_CHARACTERS))
        .collect()
}

/// Activity is checked from persisted rows, not from caller-supplied counts.
pub fn active_workers(
    agents: &[Value],
    monitors: &[studio_runtime_store::MonitorRecord],
) -> Result<u64, RuleError> {
    use std::collections::BTreeSet;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Worker {
        id: String,
        status: String,
        #[serde(default)]
        is_lead: bool,
        #[serde(default)]
        deleted_at: Option<f64>,
    }
    let mut commands = BTreeSet::new();
    for monitor in monitors {
        if monitor.status == studio_runtime_schema::studio_errors::MonitorStatus::Running
            && monitor.cancel_requested != Some(true)
        {
            commands.insert(monitor.agent.clone());
        }
    }
    let mut count = 0;
    for raw in agents {
        let worker: Worker = checked("agents", "PersistedAgentRecord", raw)?;
        if !worker.is_lead
            && !worker.deleted_at.is_some_and(|value| value != 0.0)
            && (matches!(worker.status.as_str(), "starting" | "running")
                || commands.contains(&worker.id))
        {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
pub(crate) fn complete_test_rule(record: Value) -> Value {
    let mut complete = json!({"id":"rule","agent":"owner","epoch":0,"name":"rule","kind":"interval","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":"","checks":0,"wakes":0,"inFlight":false,"fileGeneration":0,"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"livenessCommand":"","event":""});
    let Value::Object(fields) = record else {
        panic!("rule fixture must be an object")
    };
    complete.as_object_mut().unwrap().extend(fields);
    complete
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_check_wake_preserves_unicode_text() {
        let seed: Value = serde_json::from_str(include_str!("../tests/rules_engine/seed.json"))
            .expect("rule seed");
        let decision = RuleSnapshot::from_records(&seed["rule"], seed["owner"].clone())
            .expect("snapshot")
            .finish(
                1_800_000_001.0,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: "done",
                },
            )
            .expect("finish");
        assert!(decision.wake.expect("wake").text.contains("東京"));
    }

    #[test]
    fn launch_intent_requires_integer_on_decode() {
        let seed: Value = serde_json::from_str(include_str!("../tests/rules_engine/seed.json"))
            .expect("rule seed");
        let mut rule = seed["rule"].clone();
        rule["inFlight"] = json!(true);
        rule["launchIntent"] = json!({"ruleId":"rule", "checkNumber":1, "agentId":"owner",
            "epoch":7, "accountKey":"fixture", "threadId":"thread-fixture", "monitorId":"m",
            "command":"run", "timeoutMs":1000, "liveness":false});
        let snapshot = RuleSnapshot::from_records(&rule, seed["owner"].clone()).unwrap();
        assert_eq!(snapshot.launch_intent().unwrap().unwrap().check_number, 1);
        rule["launchIntent"]["checkNumber"] = json!(1.0);
        assert!(RuleSnapshot::from_records(&rule, seed["owner"].clone()).is_err());
    }

    #[test]
    fn from_records_reports_invalid_field_path() {
        let seed: Value = serde_json::from_str(include_str!("../tests/rules_engine/seed.json"))
            .expect("frozen rule seed");
        let mut rule = seed["rule"].clone();
        rule["kind"] = json!(42);
        assert!(matches!(
            RuleSnapshot::from_records(&rule, seed["owner"].clone()),
            Err(RuleError::Invalid { object, location })
                if object == "rules/rule" && !location.is_empty()
        ));
    }

    /// The persisted rule schema owns the stored `ownerHold` codes; each code
    /// of [`OwnerHold`] is one of them, and no other code is stored.
    #[test]
    fn owner_hold_codes_match_the_persisted_rule_schema() {
        let seed: Value = serde_json::from_str(include_str!("../tests/rules_engine/seed.json"))
            .expect("frozen rule seed");
        let mut rule = seed["rule"].clone();
        rule["status"] = json!("paused");
        for hold in [
            OwnerHold::FailedInputHeld,
            OwnerHold::NativeFailureHold,
            OwnerHold::Archived,
        ] {
            rule["ownerHold"] = json!(hold);
            let parsed: studio_runtime_store::RuleRecord =
                checked("rules", "PersistedRuleRecord", &rule)
                    .unwrap_or_else(|error| panic!("{hold:?}: {error}"));
            assert!(owner_paused(&parsed), "{hold:?}");
            assert_eq!(parsed.owner_hold, Some(hold));
        }
        rule["ownerHold"] = json!("unknown");
        assert!(
            checked::<studio_runtime_store::RuleRecord>("rules", "PersistedRuleRecord", &rule)
                .is_err()
        );
    }

    /// An event rule of an owner that cannot run starts no check before the
    /// tick pauses it; an idle owner's event rule still does.
    #[test]
    fn an_event_of_a_held_owner_starts_no_check() {
        let seed: Value = serde_json::from_str(include_str!("../tests/rules_engine/seed.json"))
            .expect("frozen rule seed");
        let mut rule = seed["rule"].clone();
        rule["kind"] = json!("event");
        rule["command"] = json!("check.sh");
        for (owner_change, fires) in [
            (json!({}), true),
            (json!({"archived":true}), false),
            (json!({"nativeFailureHold":true}), false),
        ] {
            let mut owner = seed["owner"].clone();
            for (field, value) in owner_change.as_object().expect("owner change") {
                owner[field] = value.clone();
            }
            let decision = RuleSnapshot::from_records(&rule, owner)
                .expect("snapshot")
                .event("worker_completed", "done", "event-1", 1_800_000_000.0)
                .expect("event decision");
            assert_eq!(decision.check.is_some(), fires, "{owner_change}");
        }
    }
}
