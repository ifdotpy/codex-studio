//! Completion and timer composition for the persisted continuation policies.
//! Decisions are staged under the caller's writer; wake ports only signal after commit.

use crate::{
    capacity_retry::{self, Action as CapacityAction, Failure as CapacityFailure},
    claude_auth_wait,
    continuation::{ContinuationError, Decision, Effect, Result, Snapshot, account, text},
    continuation_driver,
    usage_resume::{self, Cause as UsageCause, FailureObservation, LimitsObservation},
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::time::Duration;
use studio_runtime_store::{ContinuationRepository, ContinuationTable, Transaction};
use tokio::sync::watch;

const TICK_SECONDS: u64 = 15;

/// The exact settings copied by Python's `preparation_settings` helper.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparationSettings {
    model: Value,
    effort: Value,
    native_effort: Value,
    fast_mode: Value,
    yolo_mode: Value,
    profile_instructions: Value,
    role: Value,
    daybreak_enabled: Option<Value>,
    cyber_access_program: Option<Value>,
}

impl PreparationSettings {
    pub fn from_agent(agent: &Value) -> Result<Self> {
        if !agent.is_object() {
            return Err(ContinuationError::Invalid("/agent".into()));
        }
        Ok(Self {
            model: agent.get("model").cloned().unwrap_or(Value::Null),
            effort: agent.get("effort").cloned().unwrap_or(Value::Null),
            native_effort: agent.get("nativeEffort").cloned().unwrap_or(Value::Null),
            fast_mode: agent.get("fastMode").cloned().unwrap_or(Value::Null),
            yolo_mode: agent.get("yoloMode").cloned().unwrap_or(Value::Null),
            profile_instructions: agent
                .get("profileInstructions")
                .cloned()
                .unwrap_or(Value::Null),
            role: agent.get("role").cloned().unwrap_or(Value::Null),
            daybreak_enabled: agent.get("daybreakEnabled").cloned(),
            cyber_access_program: agent.get("cyberAccessProgram").cloned(),
        })
    }

    fn value(&self) -> Value {
        let mut value = json!({
            "model": self.model.clone(),
            "effort": self.effort.clone(),
            "nativeEffort": self.native_effort.clone(),
            "fastMode": self.fast_mode.clone(),
            "yoloMode": self.yolo_mode.clone(),
            "profileInstructions": self.profile_instructions.clone(),
            "role": self.role.clone(),
        });
        if let Some(enabled) = &self.daybreak_enabled {
            value["daybreakEnabled"] = enabled.clone();
        }
        if let Some(program) = &self.cyber_access_program {
            value["cyberAccessProgram"] = program.clone();
        }
        value
    }
}

/// Account data used by the local Claude auth probe. The account pin is already
/// projected by the P08 account adapter before the store transaction begins.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeAccountPin(Value);

impl ClaudeAccountPin {
    /// Keep the exact Python pin fields and reject malformed adapter projections.
    pub fn from_value(value: Value) -> Result<Self> {
        let fields = [
            "id",
            "provider",
            "home",
            "claudeOptions",
            "accountId",
            "_credentialIdentity",
            "deleted",
            "disconnected",
            "duplicateOf",
            "updatedAt",
        ];
        if !value.is_object() {
            return Err(ContinuationError::Invalid("/claude/accountPin".into()));
        }
        let value = fields
            .iter()
            .map(|field| {
                (
                    (*field).to_owned(),
                    value.get(*field).cloned().unwrap_or(Value::Null),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let value = Value::Object(value);
        let strings = ["id", "provider", "home", "accountId", "duplicateOf"];
        let booleans = ["deleted", "disconnected"];
        if !value.is_object()
            || strings
                .iter()
                .any(|key| !value.get(*key).is_none_or(Value::is_null) && !value[*key].is_string())
            || booleans
                .iter()
                .any(|key| !value.get(*key).is_none_or(Value::is_null) && !value[*key].is_boolean())
        {
            return Err(ContinuationError::Invalid("/claude/accountPin".into()));
        }
        Ok(Self(value))
    }

    fn value(&self) -> &Value {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeAuthObservation {
    pub account_pin: ClaudeAccountPin,
    pub settings: PreparationSettings,
}

impl ClaudeAuthObservation {
    pub fn checked(account_pin: ClaudeAccountPin, settings: PreparationSettings) -> Self {
        Self {
            account_pin,
            settings,
        }
    }
}

pub enum Completion<'a> {
    TerminalTurn {
        agent_id: &'a str,
        turn: &'a Value,
        known: bool,
        now: f64,
        limits: Option<&'a LimitsObservation>,
        claude_auth: Option<&'a ClaudeAuthObservation>,
    },
    ClaudeAuthTimeout {
        agent_id: &'a str,
        now: f64,
        observation: Option<&'a ClaudeAuthObservation>,
    },
}

/// One bounded scheduler batch. The host supplies its clock and the adapters that
/// read account limits/authentication outside the SQLite writer.
pub trait TickRuntime: Send + Sync + 'static {
    fn now(&self) -> f64;
    fn tick_batch(&self, now: f64) -> Result<()>;
}

/// Timer seam used by the application runtime. A batch implementation must cap
/// its row count; durable due rows are rediscovered after restart or missed wakes.
pub async fn start(runtime: impl TickRuntime, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(TICK_SECONDS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => runtime.tick_batch(runtime.now())?,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Staged {
    agent_id: String,
    epoch: i64,
    changed: bool,
    event_wake: bool,
    effects: Vec<Effect>,
}

impl Staged {
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn epoch(&self) -> i64 {
        self.epoch
    }

    pub fn changed(&self) -> bool {
        self.changed
    }

    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
}

/// The integrator maps this to the account worker's existing bounded wake lane
/// and, for queued events, the shared HTTP event wake handle.
pub trait WakePort: Send + Sync {
    fn wake_account(&self, agent_id: &str, epoch: i64) -> Result<()>;
    fn wake_events(&self, agent_id: &str, epoch: i64) -> Result<()>;
}

/// Stage all durable continuation state in the transaction that settles the
/// completion. A missing observation aborts that transaction with a named port.
pub fn stage(tx: &Transaction<'_>, completion: Completion<'_>) -> Result<Staged> {
    match completion {
        Completion::TerminalTurn {
            agent_id,
            turn,
            known,
            now,
            limits,
            claude_auth,
        } => stage_terminal(tx, agent_id, turn, known, now, limits, claude_auth),
        Completion::ClaudeAuthTimeout {
            agent_id,
            now,
            observation,
        } => stage_auth_timeout(tx, agent_id, now, observation),
    }
}

fn stage_terminal(
    tx: &Transaction<'_>,
    agent_id: &str,
    turn: &Value,
    known: bool,
    now: f64,
    limits: Option<&LimitsObservation>,
    claude_auth: Option<&ClaudeAuthObservation>,
) -> Result<Staged> {
    let mut staged = empty_stage(tx, agent_id)?;
    if !known || turn.get("id").and_then(Value::as_str).is_none() {
        return Ok(staged);
    }
    let turn_id = text(turn, "id");
    let status = text(turn, "status");
    // Check every required observation before the first write. The enclosing
    // settlement transaction can therefore handle a dependency without a
    // partially staged continuation.
    if status == "failed" {
        let error = turn
            .get("error")
            .filter(|value| !value.is_null())
            .unwrap_or(&Value::Null);
        let agent = &load(tx, agent_id, ContinuationTable::Usage, None)?.agent;
        if let Some(cause) = usage_failure(error, text(agent, "provider")) {
            if limits.is_none() {
                return Err(ContinuationError::Unavailable(
                    "usage-policy/limits-snapshot",
                ));
            }
            if cause == UsageCause::Auth
                && text(agent, "provider") == "claude"
                && claude_auth.is_none()
            {
                return Err(ContinuationError::Unavailable("claude-auth/account-pin"));
            }
        }
    }
    if status == "completed" {
        let current = load(tx, agent_id, ContinuationTable::Usage, None)?;
        let account_key = account(&current.agent);
        for (resume_id, resume_agent) in auth_wait_ids(tx, account_key)? {
            let snapshot = load(
                tx,
                &resume_agent,
                ContinuationTable::Usage,
                Some(&resume_id),
            )?;
            let decision = usage_resume::completed_proof(&snapshot, account(&snapshot.agent), now)?;
            collect_apply(
                tx,
                &mut staged,
                ContinuationTable::Usage,
                &snapshot,
                decision,
                now,
            )?;
        }
        if let Some(resume_id) = child_id(tx, agent_id, "usageResume")? {
            let snapshot = load(tx, agent_id, ContinuationTable::Usage, Some(&resume_id))?;
            let decision = usage_resume::completed_proof(&snapshot, account(&snapshot.agent), now)?;
            collect_apply(
                tx,
                &mut staged,
                ContinuationTable::Usage,
                &snapshot,
                decision,
                now,
            )?;
        }
    }

    if let Some(retry_id) = child_id(tx, agent_id, "capacityRetry")? {
        let snapshot = load(tx, agent_id, ContinuationTable::Capacity, Some(&retry_id))?;
        let decision = capacity_retry::transition(
            &snapshot,
            CapacityAction::Completed {
                turn_id,
                completed: status == "completed",
            },
            now,
        )?;
        collect_apply(
            tx,
            &mut staged,
            ContinuationTable::Capacity,
            &snapshot,
            decision,
            now,
        )?;
    }

    if status != "failed" {
        return Ok(staged);
    }
    let error = turn
        .get("error")
        .filter(|value| !value.is_null())
        .unwrap_or(&Value::Null);
    if let Some(kind) = capacity_failure(error) {
        let snapshot = load(tx, agent_id, ContinuationTable::Capacity, None)?;
        let settings = PreparationSettings::from_agent(&snapshot.agent)?.value();
        let decision = capacity_retry::failed(&snapshot, kind, turn_id, &settings, now)?;
        if let Some(record) = decision.record.as_ref()
            && record_exists(
                tx,
                ContinuationTable::Capacity,
                agent_id,
                text(record, "id"),
            )?
        {
            return Ok(staged);
        }
        collect_apply(
            tx,
            &mut staged,
            ContinuationTable::Capacity,
            &snapshot,
            decision,
            now,
        )?;
    }
    let usage_snapshot = load(tx, agent_id, ContinuationTable::Usage, None)?;
    if let Some(cause) = usage_failure(error, text(&usage_snapshot.agent, "provider")) {
        let snapshot = usage_snapshot;
        let limits = limits.ok_or(ContinuationError::Unavailable(
            "usage-policy/limits-snapshot",
        ))?;
        if cause == UsageCause::Auth
            && text(&snapshot.agent, "provider") == "claude"
            && claude_auth.is_none()
        {
            return Err(ContinuationError::Unavailable("claude-auth/account-pin"));
        }
        let reset = error.get("resetsAt").and_then(Value::as_f64);
        let decision = usage_resume::record_failure(
            &snapshot,
            FailureObservation {
                turn_id,
                cause,
                error_reset: reset,
                auth_attempt: snapshot.agent["authResumeAttempt"].as_u64().unwrap_or(0),
                limits,
            },
            now,
        )?;
        if let Some(record) = decision.record.as_ref()
            && record_exists(tx, ContinuationTable::Usage, agent_id, text(record, "id"))?
        {
            return Ok(staged);
        }
        collect_apply(
            tx,
            &mut staged,
            ContinuationTable::Usage,
            &snapshot,
            decision,
            now,
        )?;
    }
    Ok(staged)
}

fn auth_wait_ids(tx: &Transaction<'_>, account_key: &str) -> Result<Vec<(String, String)>> {
    let mut statement = tx
        .prepare(
            "SELECT id,agent FROM runtime_usage_resumes \
             WHERE json_extract(record,'$.status')='scheduled' \
               AND json_extract(record,'$.cause')='auth' \
               AND json_extract(record,'$.accountKey')=?1 \
               AND json_extract(record,'$.proofAt') IS NULL ORDER BY rowid",
        )
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?;
    statement
        .query_map([account_key], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| ContinuationError::Store(studio_runtime_store::StoreError::from(error)))
}

/// Reconcile one due usage wait under the caller's transaction. The host bounds
/// the scan to a fixed batch and fetches `limits` before opening this transaction.
pub fn stage_tick(
    tx: &Transaction<'_>,
    agent_id: &str,
    now: f64,
    limits: Option<&LimitsObservation>,
) -> Result<Staged> {
    let mut staged = empty_stage(tx, agent_id)?;
    // Capacity claims and admission remain owned by the account worker. A due
    // durable row is enough to re-signal its existing scheduler lane.
    if let Some(id) = child_id(tx, agent_id, "capacityRetry")? {
        let capacity = load(tx, agent_id, ContinuationTable::Capacity, Some(&id))?;
        if capacity.record.as_ref().is_some_and(|record| {
            text(record, "status") == "scheduled"
                && record
                    .get("dueAt")
                    .and_then(Value::as_f64)
                    .is_some_and(|due| due <= now)
        }) {
            staged.changed = true;
        }
    }
    let Some(id) = child_id(tx, agent_id, "usageResume")? else {
        return Ok(staged);
    };
    let snapshot = load(tx, agent_id, ContinuationTable::Usage, Some(&id))?;
    if snapshot.record.as_ref().is_none_or(|record| {
        text(record, "status") != "scheduled"
            || record
                .get("dueAt")
                .and_then(Value::as_f64)
                .is_none_or(|due| due > now)
    }) {
        return Ok(staged);
    }
    let limits = limits.ok_or(ContinuationError::Unavailable(
        "usage-policy/limits-snapshot",
    ))?;
    let decision = usage_resume::tick(&snapshot, limits, now)?;
    collect_apply(
        tx,
        &mut staged,
        ContinuationTable::Usage,
        &snapshot,
        decision,
        now,
    )?;
    Ok(staged)
}

fn stage_auth_timeout(
    tx: &Transaction<'_>,
    agent_id: &str,
    now: f64,
    observation: Option<&ClaudeAuthObservation>,
) -> Result<Staged> {
    let observation =
        observation.ok_or(ContinuationError::Unavailable("claude-auth/account-pin"))?;
    let snapshot = load(tx, agent_id, ContinuationTable::Usage, None)?;
    let settings = observation.settings.value();
    let pin = observation.account_pin.value();
    let Some(capture) = claude_auth_wait::capture(&snapshot, &settings, pin) else {
        return empty_stage(tx, agent_id);
    };
    let decision = claude_auth_wait::record_timeout(&snapshot, &capture, &settings, pin, now)?;
    let mut staged = empty_stage(tx, agent_id)?;
    collect_apply(
        tx,
        &mut staged,
        ContinuationTable::Usage,
        &snapshot,
        decision,
        now,
    )?;
    Ok(staged)
}

fn empty_stage(tx: &Transaction<'_>, agent_id: &str) -> Result<Staged> {
    let raw = ContinuationRepository::snapshot(tx, agent_id, ContinuationTable::Usage, None)?;
    let snapshot = Snapshot::from_records(raw)?;
    Ok(Staged {
        agent_id: agent_id.into(),
        epoch: snapshot.agent["epoch"]
            .as_i64()
            .ok_or_else(|| ContinuationError::Invalid("/agent/epoch".into()))?,
        changed: false,
        event_wake: false,
        effects: Vec::new(),
    })
}

fn load(
    tx: &Transaction<'_>,
    agent_id: &str,
    table: ContinuationTable,
    record_id: Option<&str>,
) -> Result<Snapshot> {
    Snapshot::from_records(ContinuationRepository::snapshot(
        tx, agent_id, table, record_id,
    )?)
}

fn child_id(tx: &Transaction<'_>, agent_id: &str, field: &str) -> Result<Option<String>> {
    let snapshot = load(tx, agent_id, ContinuationTable::Usage, None)?;
    Ok(snapshot.agent[field]
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

fn record_exists(
    tx: &Transaction<'_>,
    table: ContinuationTable,
    agent_id: &str,
    id: &str,
) -> Result<bool> {
    Ok(tx
        .query_row(
            &format!("SELECT 1 FROM {} WHERE id=?1 AND agent=?2", table.name()),
            [id, agent_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(studio_runtime_store::StoreError::from)?
        .is_some())
}

fn collect_apply(
    tx: &Transaction<'_>,
    staged: &mut Staged,
    table: ContinuationTable,
    snapshot: &Snapshot,
    decision: Decision,
    now: f64,
) -> Result<()> {
    let changed = decision
        .agent
        .as_ref()
        .is_some_and(|agent| agent != &snapshot.agent)
        || decision
            .record
            .as_ref()
            .is_some_and(|record| snapshot.record.as_ref() != Some(record))
        || !decision.events.is_empty()
        || !decision.metadata.is_empty()
        || !decision.effects.is_empty();
    if !changed {
        return Ok(());
    }
    continuation_driver::apply_in_transaction(tx, snapshot, table, &decision, now)?;
    staged.changed = true;
    staged.event_wake |= decision
        .effects
        .iter()
        .any(|effect| matches!(effect, Effect::Followup { .. }));
    staged.effects.extend(decision.effects);
    Ok(())
}

/// The only post-commit action: signal the existing scheduler owner.
pub fn committed(staged: &Staged, port: &dyn WakePort) -> Result<()> {
    if staged.changed {
        port.wake_account(&staged.agent_id, staged.epoch)?;
        if staged.event_wake {
            port.wake_events(&staged.agent_id, staged.epoch)?;
        }
    }
    Ok(())
}

fn capacity_failure(error: &Value) -> Option<CapacityFailure> {
    match error_kind(error) {
        "serverOverloaded" => Some(CapacityFailure::ServerOverloaded),
        "internalServerError" => Some(CapacityFailure::InternalServerError),
        "httpConnectionFailed" => Some(CapacityFailure::HttpConnectionFailed),
        "responseStreamConnectionFailed" => Some(CapacityFailure::ResponseStreamConnectionFailed),
        "responseStreamDisconnected" => Some(CapacityFailure::ResponseStreamDisconnected),
        "responseTooManyFailedAttempts" => Some(CapacityFailure::ResponseTooManyFailedAttempts),
        _ => None,
    }
}

fn usage_failure(error: &Value, provider: &str) -> Option<UsageCause> {
    match error_kind(error) {
        "usageLimitExceeded" => Some(UsageCause::UsageLimit),
        "rateLimitExceeded" => Some(UsageCause::RateLimit),
        kind if kind.to_ascii_lowercase().contains("auth") => Some(UsageCause::Auth),
        _ => {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let auth_evidence = [
                "invalid token",
                "expired token",
                "revoked token",
                "token invalid",
                "token expired",
                "token revoked",
                "invalid key",
                "expired key",
                "revoked key",
                "key invalid",
                "key expired",
                "key revoked",
                "sign in",
                "sign-in",
                "signin",
                "authentication required",
                "unauthorized",
            ]
            .iter()
            .any(|needle| message.contains(needle));
            (message.contains("401")
                || (message.contains("403") && auth_evidence)
                || (provider == "claude"
                    && (message.contains("oauth session expired")
                        || message.contains("failed to authenticate"))))
            .then_some(UsageCause::Auth)
        }
    }
}

fn error_kind(error: &Value) -> &str {
    error["codexErrorInfo"]
        .as_str()
        .or_else(|| {
            error["codexErrorInfo"]
                .as_object()
                .and_then(|value| value.keys().next().map(String::as_str))
        })
        .unwrap_or("")
}
