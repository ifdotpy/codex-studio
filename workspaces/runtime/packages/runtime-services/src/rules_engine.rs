//! Rule decisions on checked persisted records. No clock, filesystem, store or provider I/O.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::event_write::{python_json, python_strip, truthy};

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
/// Why a rule is paused while its owner cannot act on a wake. The code is the
/// rule's `ownerHold`; the reason is its `error`. The tick resumes the rule
/// when the owner can run again; a manual pause or resume removes the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerHold {
    /// The owner's last start failed and kept its input pending.
    FailedInput,
    /// The owner is held after a native failure; no wake starts it.
    NativeFailure,
    /// The owner's conversation is archived.
    Archived,
}
impl OwnerHold {
    const ALL: [Self; 3] = [Self::FailedInput, Self::NativeFailure, Self::Archived];
    fn code(self) -> &'static str {
        match self {
            Self::FailedInput => "failed_input_held",
            Self::NativeFailure => "native_failure_hold",
            Self::Archived => "archived",
        }
    }
    fn reason(self) -> &'static str {
        match self {
            Self::FailedInput => {
                "Paused: the agent failed and its input waits, so it cannot act on a wake. The rule resumes when the agent can run again."
            }
            Self::NativeFailure => {
                "Paused: the agent is held after a failure, so it cannot act on a wake. The rule resumes when the agent can run again."
            }
            Self::Archived => {
                "Paused: the conversation of the agent is archived. The rule resumes when the conversation is restored."
            }
        }
    }
    fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|hold| hold.code() == code)
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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Interval,
    Once,
    File,
    Event,
    LowWorkers,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Active,
    Paused,
    Completed,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rule {
    id: String,
    agent: String,
    epoch: u64,
    name: String,
    kind: Kind,
    status: Status,
    interval_seconds: u64,
    next_at: f64,
    created: f64,
    command: String,
    text: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    event: String,
    #[serde(default)]
    in_flight: bool,
    #[serde(default)]
    checks: u64,
    #[serde(default)]
    wakes: u64,
    #[serde(default)]
    fingerprint: Option<(i64, u64, u64)>,
    #[serde(default)]
    file_activity_at: Option<f64>,
    #[serde(default)]
    file_generation: u64,
    #[serde(default = "minus_one")]
    stall_wake_generation: i64,
    #[serde(default = "default_stall")]
    stall_timeout_seconds: u64,
    #[serde(default)]
    liveness_command: String,
    #[serde(default)]
    stall_probe: bool,
    #[serde(default)]
    stall_event_key: Option<String>,
    #[serde(default)]
    minimum_workers: Option<u64>,
    #[serde(default)]
    duration_minutes: Option<u64>,
    #[serde(default)]
    low_since: Option<f64>,
    #[serde(default)]
    alerted: bool,
    #[serde(default)]
    last_event: Option<String>,
    #[serde(default)]
    event_text: String,
    #[serde(default)]
    owner_hold: Option<String>,
}
impl Rule {
    /// Only an active rule without a check in flight can change in a tick.
    fn can_tick(&self) -> bool {
        self.status == Status::Active && !self.in_flight
    }
    /// A rule that the tick paused while its owner cannot run; the tick
    /// resumes it when the owner can.
    fn owner_paused(&self) -> bool {
        self.status == Status::Paused && !self.in_flight && self.owner_hold.is_some()
    }
}

/// Whether a tick can change the stored rule `rule`, which is validated as in
/// [`RuleSnapshot::from_records`]. [`RuleSnapshot::tick`] decides nothing for
/// any other rule, so its owner record need not be read.
pub fn tick_can_change(rule: &Value) -> Result<bool, RuleError> {
    let parsed: Rule = checked("rules", "PersistedRuleRecord", rule)?;
    Ok(parsed.can_tick() || parsed.owner_paused())
}

fn minus_one() -> i64 {
    -1
}
fn default_stall() -> u64 {
    DEFAULT_RULE_STALL_SECONDS
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
            Some(OwnerHold::NativeFailure)
        } else if self.status == "failed" && held_input {
            Some(OwnerHold::FailedInput)
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
    raw: Value,
    owner_raw: Value,
    rule: Rule,
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
    pub record: Option<Value>,
    pub wake: Option<RuleWake>,
    pub check: Option<RuleCheck>,
}

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
    pub fn from_records(rule: Value, owner: Value) -> Result<Self, RuleError> {
        let parsed: Rule = checked("rules", "PersistedRuleRecord", &rule)?;
        let agent: Owner = checked("agents", "PersistedAgentRecord", &owner)?;
        if parsed.agent != agent.id {
            return Err(invalid("rules", &rule, "/agent"));
        }
        if parsed.kind == Kind::File && parsed.path.is_none() {
            return Err(invalid("rules", &rule, "/path"));
        }
        if parsed.kind == Kind::LowWorkers
            && (parsed.minimum_workers.is_none() || parsed.duration_minutes.is_none())
        {
            return Err(invalid("rules", &rule, "/minimumWorkers"));
        }
        Ok(Self {
            raw: rule,
            owner_raw: owner,
            rule: parsed,
            owner: agent,
        })
    }

    pub fn id(&self) -> &str {
        &self.rule.id
    }
    pub fn owner_id(&self) -> &str {
        &self.rule.agent
    }
    pub fn record(&self) -> &Value {
        &self.raw
    }
    pub fn is_low_workers(&self) -> bool {
        self.rule.kind == Kind::LowWorkers
    }
    /// The owner id and epoch whose held input a tick must observe: only a
    /// failed owner can hold its input, and only a rule that can change.
    pub fn held_input_owner(&self) -> Option<(&str, u64)> {
        (self.owner.status == "failed" && (self.rule.can_tick() || self.rule.owner_paused()))
            .then_some((self.owner.id.as_str(), self.owner.epoch))
    }
    pub fn current(&self, rule: &Value, owner: &Value) -> bool {
        rule == &self.raw
            && OWNER_FIELDS
                .iter()
                .all(|field| owner.get(field) == self.owner_raw.get(field))
    }
    fn stalled(&self, now: f64) -> bool {
        let r = &self.rule;
        r.kind == Kind::File
            && r.stall_timeout_seconds != 0
            && now - r.file_activity_at.unwrap_or(r.created) >= r.stall_timeout_seconds as f64
            && r.file_generation as i64 > r.stall_wake_generation
    }
    pub fn fingerprint_path(&self, now: f64) -> Option<&str> {
        (self.rule.kind == Kind::File
            && self.rule.status == Status::Active
            && !self.rule.in_flight
            && !self.owner.deleted()
            && self.owner.auto_wake
            && self.owner.epoch == self.rule.epoch
            && !self.owner.recovery_pending(self.rule.epoch)
            && (self.stalled(now) || self.rule.next_at <= now))
            .then_some(self.rule.path.as_deref())
            .flatten()
    }
    fn with_record(&self, record: Value) -> Result<Self, RuleError> {
        Self::from_records(record, self.owner_raw.clone())
    }
    fn change(&self, record: Value) -> RuleDecision {
        RuleDecision {
            record: Some(record),
            ..RuleDecision::default()
        }
    }

    /// Rechecked by the effect adapter before submitting a durable launch intent.
    pub fn launch_allowed(&self, check: &RuleCheck) -> bool {
        self.rule.status == Status::Active
            && self.rule.in_flight
            && self.owner.auto_wake
            && !self.owner.deleted()
            && self.owner.epoch == check.epoch
            && !self.owner.recovery_pending(check.epoch)
            && &self.check() == check
    }

    pub fn check(&self) -> RuleCheck {
        let r = &self.rule;
        RuleCheck {
            rule_id: r.id.clone(),
            check_number: r.checks,
            agent_id: r.agent.clone(),
            epoch: r.epoch,
            account_key: self.owner.account_key.clone(),
            thread_id: self.owner.thread_id.clone(),
            monitor_id: Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("rule:{}:{}", r.id, r.checks).as_bytes(),
            )
            .to_string(),
            command: if r.stall_probe {
                &r.liveness_command
            } else {
                &r.command
            }
            .clone(),
            timeout_ms: r
                .interval_seconds
                .saturating_mul(1000)
                .clamp(MIN_COMMAND_MILLISECONDS, MAX_COMMAND_MILLISECONDS),
            liveness: r.stall_probe,
        }
    }

    pub fn tick(&self, now: f64, observation: TickObservation) -> Result<RuleDecision, RuleError> {
        finite_time(now)?;
        let r = &self.rule;
        if r.owner_paused() {
            return Ok(self.resume_after_owner_hold(observation.held_input));
        }
        if !r.can_tick() {
            return Ok(RuleDecision::default());
        }
        if !self.owner.deleted()
            && self.owner.epoch == r.epoch
            && self.owner.recovery_pending(r.epoch)
        {
            return Ok(RuleDecision::default());
        }
        let mut next = self.raw.clone();
        if self.owner.deleted() || !self.owner.auto_wake || self.owner.epoch != r.epoch {
            next["status"] = json!("paused");
            next["error"] = json!(STOPPED);
            return Ok(self.change(next));
        }
        // An owner that cannot act on a wake gets no new check or wake; each
        // one would only start a command and leave its result unread.
        if let Some(hold) = self.owner.hold(observation.held_input) {
            next["status"] = json!("paused");
            next["ownerHold"] = json!(hold.code());
            next["error"] = json!(hold.reason());
            return Ok(self.change(next));
        }
        if r.kind == Kind::LowWorkers {
            return self.low_workers(now, observation.active_workers);
        }
        if r.kind == Kind::Event {
            return Ok(RuleDecision::default());
        }
        if self.stalled(now) {
            let fingerprint = observation
                .fingerprint
                .ok_or(RuleError::Observation("file fingerprint missing"))?;
            if fingerprint != r.fingerprint {
                next["fingerprint"] = json!(fingerprint);
                next["fileActivityAt"] = json!(now);
                next["fileGeneration"] = json!(r.file_generation + 1);
                return Ok(self.change(next));
            }
            let text = python_json(
                &json!({"rule":r.id,"name":r.name,"message":"file unchanged",
                "path":r.path,"quietSeconds":(now-r.file_activity_at.unwrap_or(now)) as i64}),
            );
            let id = format!("rule-stall:{}:{}", r.id, r.file_generation);
            if !r.liveness_command.is_empty() {
                next["inFlight"] = json!(true);
                next["checks"] = json!(r.checks + 1);
                next["stallProbe"] = json!(true);
                next["stallEventKey"] = json!(id);
                next["stallText"] = json!(text);
                return self.launch(next, now);
            }
            next["stallWakeGeneration"] = json!(r.file_generation);
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
        if r.next_at > now {
            return Ok(RuleDecision::default());
        }
        if r.kind == Kind::Once && now - r.next_at > ONCE_RULE_GRACE_SECONDS {
            let message = "Missed scheduled run. Resume this rule to schedule a new run, or delete it to cancel.";
            next["status"] = json!("paused");
            next["error"] = json!(message);
            next["wakes"] = json!(r.wakes + 1);
            return Ok(RuleDecision {
                record: Some(next),
                wake: Some(RuleWake {
                    id: format!("rule-missed:{}:{}:{}", r.id, r.epoch, r.next_at),
                    kind: "rule_missed",
                    text: python_json(
                        &json!({"rule":r.id,"name":r.name,"dueAt":r.next_at,"delaySeconds":now-r.next_at,"message":message}),
                    ),
                }),
                check: None,
            });
        }
        next["nextAt"] = json!(now + r.interval_seconds as f64);
        if r.kind == Kind::File {
            let fingerprint = observation
                .fingerprint
                .ok_or(RuleError::Observation("file fingerprint missing"))?;
            next["fingerprint"] = json!(fingerprint);
            if fingerprint == r.fingerprint {
                return Ok(self.change(next));
            }
            next["fileActivityAt"] = json!(now);
            next["fileGeneration"] = json!(r.file_generation + 1);
        }
        next["inFlight"] = json!(true);
        next["lastAt"] = json!(now);
        next["checks"] = json!(r.checks + 1);
        self.launch(next, now)
    }

    /// Resume a rule that the tick paused for its owner once the owner can
    /// run again. A stopped or deleted owner turns it into an explicit pause.
    fn resume_after_owner_hold(&self, held_input: bool) -> RuleDecision {
        let r = &self.rule;
        let mut next = self.raw.clone();
        let stopped = self.owner.deleted() || !self.owner.auto_wake || self.owner.epoch != r.epoch;
        if !stopped && let Some(hold) = self.owner.hold(held_input) {
            if r.owner_hold.as_deref().and_then(OwnerHold::from_code) == Some(hold) {
                return RuleDecision::default();
            }
            next["ownerHold"] = json!(hold.code());
            next["error"] = json!(hold.reason());
            return self.change(next);
        }
        if let Some(object) = next.as_object_mut() {
            object.remove("ownerHold");
        }
        if stopped {
            next["error"] = json!(STOPPED);
            return self.change(next);
        }
        next["status"] = json!("active");
        next["error"] = Value::Null;
        if r.kind == Kind::LowWorkers {
            next["lowSince"] = Value::Null;
            next["alerted"] = json!(false);
        }
        self.change(next)
    }

    // No-command checks finish in the same durable decision; a crash cannot strand their wake.
    fn launch(&self, next: Value, now: f64) -> Result<RuleDecision, RuleError> {
        let snapshot = self.with_record(next)?;
        if snapshot.rule.status != Status::Active
            || !snapshot.owner.auto_wake
            || snapshot.rule.epoch != snapshot.owner.epoch
        {
            let mut next = snapshot.raw;
            next["inFlight"] = json!(false);
            return Ok(self.change(next));
        }
        if snapshot.check().command.is_empty() {
            snapshot.finish(
                now,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: &snapshot.rule.event_text,
                },
            )
        } else {
            Ok(RuleDecision {
                check: Some(snapshot.check()),
                ..self.change(snapshot.raw)
            })
        }
    }

    fn low_workers(&self, now: f64, count: Option<u64>) -> Result<RuleDecision, RuleError> {
        let r = &self.rule;
        let count = count.ok_or(RuleError::Observation("worker activity missing"))?;
        let minimum = r
            .minimum_workers
            .ok_or(RuleError::Observation("minimum workers missing"))?;
        let duration = r
            .duration_minutes
            .ok_or(RuleError::Observation("worker duration missing"))?;
        let mut next = self.raw.clone();
        next["activeWorkers"] = json!(count);
        if count >= minimum {
            next["lowSince"] = Value::Null;
            next["alerted"] = json!(false);
        } else {
            let since = r.low_since.filter(|since| now >= *since).unwrap_or(now);
            next["lowSince"] = json!(since);
            let elapsed = now - since;
            if !r.alerted && elapsed > (duration * 60) as f64 {
                next["alerted"] = json!(true);
                next["lastAt"] = json!(now);
                next["checks"] = json!(r.checks + 1);
                let output =
                    python_json(&json!({"activeSubagents":count,"minimumSubagents":minimum,
                    "belowThresholdSince":since,"elapsedMinutes":elapsed/60.0}));
                return self.with_record(next)?.finish(
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
        let r = &self.rule;
        if r.kind != Kind::Event
            || r.event != alias
            || r.status != Status::Active
            || r.in_flight
            || r.epoch != self.owner.epoch
            || r.last_event.as_deref() == Some(key)
            // The tick pauses this rule; until then an event starts nothing.
            || self.owner.hold(false).is_some()
        {
            return Ok(RuleDecision::default());
        }
        let mut next = self.raw.clone();
        next["inFlight"] = json!(true);
        next["lastEvent"] = json!(key);
        next["lastAt"] = json!(now);
        next["checks"] = json!(r.checks + 1);
        next["eventText"] = json!(text.chars().take(OUTPUT_CHARACTERS).collect::<String>());
        self.launch(next, now)
    }

    pub fn finish(&self, now: f64, result: CheckResult<'_>) -> Result<RuleDecision, RuleError> {
        finite_time(now)?;
        let r = &self.rule;
        let mut next = self.raw.clone();
        let output = tail(result.output);
        let mut wake = None;
        let authorized = r.status == Status::Active
            && self.owner.epoch == r.epoch
            && self.owner.finish_allowed();
        if r.stall_probe {
            if authorized && !self.owner.deleted() {
                let id = r
                    .stall_event_key
                    .clone()
                    .ok_or_else(|| invalid("rules", &self.raw, "/stallEventKey"))?;
                wake = Some(RuleWake {
                    id,
                    kind: "rule_stall",
                    text: python_json(&json!({
                    "rule":r.id,"name":r.name,"message":"file unchanged","path":r.path,
                    "quietSeconds":(now-r.file_activity_at.unwrap_or(now)) as i64,
                    "livenessResult":output,"livenessExitCode":result.code,"livenessError":result.error})),
                });
            }
            next["inFlight"] = json!(false);
            next["stallProbe"] = json!(false);
            next["stallWakeGeneration"] = json!(r.file_generation);
            next["lastStallFinished"] = json!(now);
            next["lastStallExitCode"] = json!(result.code);
            next["lastStallError"] = json!(result.error);
        } else {
            let suppressed = python_strip(result.output)
                .rsplit([
                    '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}',
                    '\u{2028}', '\u{2029}',
                ])
                .next()
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
                .is_some_and(|payload| payload.get("wakeAgent") == Some(&Value::Bool(false)));
            next["inFlight"] = json!(false);
            next["lastExitCode"] = json!(result.code);
            next["error"] = json!(result.error);
            next["lastOutput"] = json!(output);
            next["lastFinished"] = json!(now);
            if result.code == Some(0)
                && result.error.is_none_or(str::is_empty)
                && !suppressed
                && authorized
            {
                wake = Some(RuleWake {
                    id: format!("rule-wake:{}:{}", r.id, r.checks),
                    kind: "rule",
                    text: python_json(
                        &json!({"rule":r.id,"name":r.name,"instruction":r.text,"output":output}),
                    ),
                });
                next["wakes"] = json!(r.wakes + 1);
            }
            if r.kind == Kind::Once && r.status == Status::Active {
                next["status"] = json!("completed");
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
        let r = &self.rule;
        let mut next = self.raw.clone();
        if r.kind == Kind::File
            && !self
                .raw
                .as_object()
                .is_some_and(|object| object.contains_key("fileActivityAt"))
        {
            next["fileActivityAt"] = json!(now);
            next["fileGeneration"] = json!(0);
            next["stallWakeGeneration"] = json!(-1);
            next["stallTimeoutSeconds"] = json!(DEFAULT_RULE_STALL_SECONDS);
            next["livenessCommand"] = json!("");
        }
        if r.kind == Kind::LowWorkers {
            next["lowSince"] = Value::Null;
            next["nextAt"] = json!(now);
        }
        if r.in_flight {
            if r.status == Status::Active {
                next["restartCheck"] =
                    json!({"epoch":r.epoch,"checks":r.checks,"monitorId":self.check().monitor_id});
            } else if let Some(object) = next.as_object_mut() {
                object.remove("restartCheck");
            }
            next["inFlight"] = json!(false);
            next["status"] = json!("paused");
            next["error"] = json!(RESTARTED);
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
            || check.rule_id != self.rule.id
            || check.agent_id != self.rule.agent
        {
            return Ok(RuleDecision::default());
        }
        // The reservation records the exact check submitted to the monitor. A rule may be
        // paused while that monitor is still live, so settle that durable intent even when
        // derived settings have since changed. Without a reservation, compare the snapshot.
        let matches_check = match self
            .raw
            .get("launchIntent")
            .filter(|value| !value.is_null())
        {
            Some(intent) => serde_json::from_value::<RuleCheck>(intent.clone())
                .is_ok_and(|intent| intent == *check),
            None => self.check() == *check,
        };
        if !matches_check {
            return Ok(RuleDecision::default());
        }
        if self.rule.in_flight {
            return self.finish(now, result);
        }
        // Python records a canceled monitor's result after the owning rule has
        // already been paused. Keep that receipt while leaving the rule paused.
        if self.rule.status == Status::Paused && result.cancelled {
            return self.finish(now, result);
        }
        let marker =
            json!({"epoch":check.epoch,"checks":check.check_number,"monitorId":check.monitor_id});
        if self.raw.get("restartCheck") != Some(&marker)
            || self.rule.status != Status::Paused
            || self.owner.epoch != check.epoch
            || self.owner.deleted()
        {
            return Ok(RuleDecision::default());
        }
        let mut next = self.raw.clone();
        if let Some(object) = next.as_object_mut() {
            object.remove("restartCheck");
        }
        next["status"] = json!("active");
        next["error"] = Value::Null;
        self.with_record(next)?.finish(now, result)
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
pub fn active_workers(agents: &[Value], monitors: &[Value]) -> Result<u64, RuleError> {
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
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Monitor {
        agent: String,
        status: String,
        #[serde(default)]
        cancel_requested: bool,
    }
    let mut commands = BTreeSet::new();
    for raw in monitors {
        let monitor: Monitor = checked("monitors", "PersistedMonitorRecord", raw)?;
        if monitor.status == "running" && !monitor.cancel_requested {
            commands.insert(monitor.agent);
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
mod tests {
    use super::*;

    /// The persisted rule schema owns the stored `ownerHold` codes; each code
    /// of [`OwnerHold`] is one of them, and no other code is stored.
    #[test]
    fn owner_hold_codes_match_the_persisted_rule_schema() {
        let seed: Value = serde_json::from_str(include_str!("../tests/rules_engine/seed.json"))
            .expect("frozen rule seed");
        let mut rule = seed["rule"].clone();
        rule["status"] = json!("paused");
        for hold in OwnerHold::ALL {
            rule["ownerHold"] = json!(hold.code());
            let parsed: Rule = checked("rules", "PersistedRuleRecord", &rule)
                .unwrap_or_else(|error| panic!("{hold:?}: {error}"));
            assert!(parsed.owner_paused(), "{hold:?}");
            assert_eq!(OwnerHold::from_code(hold.code()), Some(hold));
        }
        rule["ownerHold"] = json!("unknown");
        assert!(checked::<Rule>("rules", "PersistedRuleRecord", &rule).is_err());
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
            let decision = RuleSnapshot::from_records(rule.clone(), owner)
                .expect("snapshot")
                .event("worker_completed", "done", "event-1", 1_800_000_000.0)
                .expect("event decision");
            assert_eq!(decision.check.is_some(), fires, "{owner_change}");
        }
    }
}
