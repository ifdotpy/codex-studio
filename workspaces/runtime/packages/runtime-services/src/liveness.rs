//! The liveness checker: invariants that find a stuck chat before its owner
//! does. Each finding is a row of the Studio errors list with a code and a
//! plain reason. Detection only reads stored state. It never settles a turn,
//! wakes a chat, restarts a session, or sends a provider request:
//! - `turn_stalled`: an active native turn whose provider sent nothing for
//!   [`SILENT_TURN_SECONDS`] while no tool runs.
//!   A manual delivery hold does not stop this turn or hide its stall.
//! - `input_not_started`: an owner message that waits longer than
//!   [`PENDING_INPUT_SECONDS`] while its chat is idle. A held chat's row says why.
//! - `session_not_started`: chats that wait for their account session
//!   (`startWait.reason` `provider_session`) longer than
//!   [`SESSION_START_SECONDS`]. The row names the account and the last start
//!   error that the account registry recorded.
//! - `schedule_stalled`: a usage resume or capacity retry that did not move
//!   [`SCHEDULE_OVERDUE_SECONDS`] after its due time, or an account transfer
//!   that did not change for that long while a member waits for Studio or for
//!   the owner.
use rusqlite::OptionalExtension;
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, SchedulerRepository, StoreError, Transaction,
};
// Liveness reads only observations, not the stricter scheduler or transfer
// records. Invalid field types retain the historical indexing defaults.
trait Observation: Default {
    fn observes(name: &str) -> bool;
    fn field(&mut self, name: &str, value: Value);
}

struct ObservationVisitor<T>(PhantomData<T>);
impl<'de, T: Observation> Visitor<'de> for ObservationVisitor<T> {
    type Value = T;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a liveness observation")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<T, A::Error> {
        let mut result = T::default();
        while let Some(name) = map.next_key::<String>()? {
            if T::observes(&name) {
                result.field(&name, map.next_value()?);
            } else {
                map.next_value::<Value>()?;
            }
        }
        Ok(result)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<T, A::Error> {
        while seq.next_element::<Value>()?.is_some() {}
        Ok(T::default())
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<T, E> {
        Ok(T::default())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<T, E> {
        Ok(T::default())
    }
}
macro_rules! deserialize_observation {
    ($record:ty) => {
        impl<'de> Deserialize<'de> for $record {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_any(ObservationVisitor::<Self>(PhantomData))
            }
        }
    };
}
fn string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

#[derive(Default)]
struct SessionFailure {
    reason: Option<String>,
}
impl Observation for SessionFailure {
    fn observes(name: &str) -> bool {
        name == "reason"
    }
    fn field(&mut self, _: &str, value: Value) {
        self.reason = string(&value);
    }
}
deserialize_observation!(SessionFailure);

#[derive(Default)]
struct ScheduleObservation {
    id: Option<String>,
    due_at: Option<f64>,
    claimed_at: Option<f64>,
    attempt: Option<u64>,
    max_attempts: Option<u64>,
    reason: Option<String>,
}
impl Observation for ScheduleObservation {
    fn observes(name: &str) -> bool {
        matches!(
            name,
            "id" | "dueAt" | "claimedAt" | "attempt" | "maxAttempts" | "reason"
        )
    }
    fn field(&mut self, name: &str, value: Value) {
        match name {
            "id" => self.id = string(&value),
            "dueAt" => self.due_at = value.as_f64(),
            "claimedAt" => self.claimed_at = value.as_f64(),
            "attempt" => self.attempt = value.as_u64(),
            "maxAttempts" => self.max_attempts = value.as_u64(),
            "reason" => self.reason = string(&value),
            _ => {}
        }
    }
}
deserialize_observation!(ScheduleObservation);

#[derive(Default)]
struct MemberObservation {
    phase: Option<String>,
    name: Option<String>,
    error: Option<String>,
}
#[derive(Default)]
struct TransferObservation {
    lead_id: Option<String>,
    target: Option<String>,
    updated: Option<f64>,
    created: Option<f64>,
    members: BTreeMap<String, MemberObservation>,
}
impl Observation for TransferObservation {
    fn observes(name: &str) -> bool {
        matches!(
            name,
            "leadId" | "targetAccountKey" | "updated" | "created" | "members"
        )
    }
    fn field(&mut self, name: &str, value: Value) {
        match name {
            "leadId" => self.lead_id = string(&value),
            "targetAccountKey" => self.target = string(&value),
            "updated" => self.updated = value.as_f64(),
            "created" => self.created = value.as_f64(),
            "members" => {
                self.members = value
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(id, row)| {
                        (
                            id.clone(),
                            MemberObservation {
                                phase: string(&row["phase"]),
                                name: string(&row["name"]),
                                error: string(&row["error"]),
                            },
                        )
                    })
                    .collect()
            }
            _ => {}
        }
    }
}
deserialize_observation!(TransferObservation);

/// Unit: seconds without a provider event before a running turn is stalled.
pub const SILENT_TURN_SECONDS: f64 = 600.0;
/// Unit: seconds an owner message waits in an idle chat before it is stuck.
pub const PENDING_INPUT_SECONDS: f64 = 120.0;
/// Unit: seconds chats wait for their account session before it is stuck.
pub const SESSION_START_SECONDS: f64 = 120.0;
/// Unit: seconds after its due time before a schedule is stuck.
pub const SCHEDULE_OVERDUE_SECONDS: f64 = 900.0;
/// The invariant a finding breaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    TurnStalled,
    InputNotStarted,
    SessionNotStarted,
    ScheduleStalled,
}

/// One stuck state, for one row of the Studio errors list.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub code: Code,
    /// Row identity; a later episode of the same stall has another one.
    pub id: String,
    pub agent: String,
    pub agent_name: String,
    /// The specific cause in plain words.
    pub detail: String,
    /// What is stuck: a command, a message, an account or a schedule.
    pub subject: String,
    pub event: Option<String>,
    /// Unix seconds when the stall began.
    pub first_at: f64,
    /// Unix seconds of the latest evidence.
    pub at: f64,
}

/// Record why the session of `account` did not start. The account registry
/// calls this on a start failure and [`clear_session_start_failure`] once the
/// session runs.
pub fn record_session_start_failure(
    tx: &Transaction<'_>,
    account: &str,
    reason: &str,
    now: f64,
) -> Result<(), StoreError> {
    let current = session_start_failure(tx, account)?;
    if current
        .as_ref()
        .is_some_and(|value| value.reason.as_deref() == Some(reason))
    {
        return Ok(());
    }
    studio_runtime_store::account_state::ensure_account(tx, account)?;
    tx.execute(
        "UPDATE runtime_account_state SET start_failure=?2 WHERE account=?1",
        rusqlite::params![account, json!({"reason":reason,"at":now}).to_string()],
    )?;
    Ok(())
}

/// Forget the start error of `account` once its session runs.
pub fn clear_session_start_failure(tx: &Transaction<'_>, account: &str) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE runtime_account_state SET start_failure=NULL WHERE account=?1",
        [account],
    )?;
    Ok(())
}

fn session_start_failure(
    db: &studio_runtime_store::Connection,
    account: &str,
) -> Result<Option<SessionFailure>, StoreError> {
    let raw: Option<String> = db
        .query_row(
            "SELECT start_failure FROM runtime_account_state WHERE account=?1",
            [account],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    raw.map(|raw| serde_json::from_str(&raw).map_err(StoreError::from))
        .transpose()
}

/// Unix seconds of the newest provider event of a running chat, from its
/// merged volatile fields; `None` when it has none.
pub fn last_provider_activity(record: &Value) -> Option<f64> {
    let activity = record["activity"]["at"]
        .as_f64()
        .filter(|at| *at > 0.0 && at.is_finite());
    let event = record["lastEvent"]
        .as_str()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.timestamp_millis() as f64 / 1000.0);
    match (activity, event) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Whether an active native turn's provider is silent while no tool runs.
pub fn silent_turn(record: &Value, now: f64) -> Option<f64> {
    let active = record["status"] == "running"
        || (record["status"] == "paused"
            && AgentRecord::from_value(record.clone())
                .ok()
                .is_some_and(|agent| SchedulerRepository::manual_delivery_hold_tracks(&agent)));
    if !active
        || record["inFlight"] != true
        || record["turnId"].as_str().is_none_or(str::is_empty)
        || record["activeTools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    {
        return None;
    }
    let since = last_provider_activity(record)
        .or_else(|| record["startAttempt"]["created"].as_f64())
        .or_else(|| record["created"].as_f64())?;
    (now - since >= SILENT_TURN_SECONDS).then_some(since)
}

/// Every current finding, newest first is the caller's order.
pub fn findings(tx: &Transaction<'_>, now: f64) -> Result<Vec<Finding>, StoreError> {
    let mut found = Vec::new();
    stalled_turns(tx, now, &mut found)?;
    waiting_inputs(tx, now, &mut found)?;
    waiting_sessions(tx, now, &mut found)?;
    stalled_schedules(tx, now, &mut found)?;
    for finding in &mut found {
        if let Some(record) = merged_agent(tx, &finding.agent)? {
            finding.detail.push_str(&format!(
                " Account: {}. Start: {} (start of the wait). Last provider event: {}.",
                record["accountKey"].as_str().unwrap_or("default"),
                timestamp(Some(finding.first_at)),
                timestamp(last_provider_activity(&record)),
            ));
        }
    }
    Ok(found)
}

fn timestamp(at: Option<f64>) -> String {
    at.filter(|at| at.is_finite())
        .and_then(|at| chrono::DateTime::from_timestamp(at as i64, 0))
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "unknown".into())
}

fn agent_name(record: &Value) -> String {
    record["name"].as_str().unwrap_or("Agent").to_owned()
}

fn merged_agent(tx: &Transaction<'_>, id: &str) -> Result<Option<Value>, StoreError> {
    let Some(record) = AgentRepository::get(tx, &AgentId::from(id))? else {
        return Ok(None);
    };
    let mut value = serde_json::to_value(record)?;
    studio_runtime_store::agent_volatile::merge(tx, id, &mut value)?;
    Ok(Some(value))
}

fn minutes(seconds: f64) -> i64 {
    (seconds / 60.0).floor() as i64
}

fn stalled_turns(
    tx: &Transaction<'_>,
    now: f64,
    found: &mut Vec<Finding>,
) -> Result<(), StoreError> {
    let mut statement = tx.prepare_cached(
        "SELECT id FROM runtime_agents WHERE deleted_at IS NULL AND status IN ('running','paused') AND COALESCE(in_flight,0)=1",
    )?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in ids {
        let Some(record) = merged_agent(tx, &id)? else {
            continue;
        };
        let Some(since) = silent_turn(&record, now) else {
            continue;
        };
        let turn = record["turnId"].as_str().unwrap_or_default();
        let detail = format!(
            "The provider sent nothing for {} minutes, and no tool runs. The turn outcome is unknown.",
            minutes(now - since)
        );
        found.push(Finding {
            code: Code::TurnStalled,
            id: format!("liveness:turn_stalled:{id}:{turn}"),
            agent: id.clone(),
            agent_name: agent_name(&record),
            detail,
            subject: String::new(),
            event: None,
            first_at: since,
            at: now,
        });
    }
    Ok(())
}

/// Why a chat with a waiting message does not start by itself.
fn input_hold(record: &Value) -> Option<String> {
    let status = record["status"].as_str().unwrap_or_default();
    if record["autoWake"] != true {
        return Some(
            "The chat is stopped and does not start by itself. Resume it to send the message."
                .into(),
        );
    }
    if studio_runtime_store::truthy(record.get("nativeFailureHold")) {
        return Some("The chat is held after a provider failure. Open it to resume.".into());
    }
    if let Some(hold) = record
        .get("startOutcomeHold")
        .filter(|hold| hold.is_object())
    {
        return Some(format!(
            "The chat waits for the outcome of an earlier start: {}",
            hold["reason"].as_str().unwrap_or("its outcome is unknown")
        ));
    }
    if record
        .get("accountTransferId")
        .is_some_and(|transfer| !transfer.is_null())
    {
        return Some("The chat waits for its account transfer.".into());
    }
    match status {
        "failed" => Some(format!(
            "The chat failed: {}",
            record["error"].as_str().unwrap_or("no error was recorded")
        )),
        "paused" => Some("The chat is paused. Resume it to send the message.".into()),
        _ => None,
    }
}

fn waiting_inputs(
    tx: &Transaction<'_>,
    now: f64,
    found: &mut Vec<Finding>,
) -> Result<(), StoreError> {
    // A message with a stored start error is already a `input_stuck` row.
    let mut statement = tx.prepare_cached(
        "SELECT e.agent,e.id,e.created,COUNT(*) OVER (PARTITION BY e.agent) FROM runtime_events e \
         JOIN runtime_agents a ON a.id=e.agent \
         WHERE e.status='pending' AND e.kind='user' AND e.created<?1 AND COALESCE(e.error,'')='' \
         AND a.deleted_at IS NULL AND e.epoch=json_extract(a.record,'$.epoch') \
         AND a.status NOT IN ('running','starting','queued','approval') \
         ORDER BY e.agent,e.created,e.id",
    )?;
    let rows = statement
        .query_map([now - PENDING_INPUT_SECONDS], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut oldest = BTreeMap::new();
    for (agent, event, created, count) in rows {
        oldest.entry(agent).or_insert((event, created, count));
    }
    for (agent, (event, created, count)) in oldest {
        let Some(record) = merged_agent(tx, &agent)? else {
            continue;
        };
        if studio_runtime_store::truthy(record.get("remoteWorker"))
            || studio_runtime_store::truthy(record.get("remoteAnchor"))
        {
            continue;
        }
        let hold = input_hold(&record);
        let waiting = if count > 1 {
            format!("{count} messages wait. ")
        } else {
            String::new()
        };
        let detail = match hold {
            Some(hold) => format!("{waiting}{hold}"),
            None => format!("{waiting}The chat is idle. Its message has not started."),
        };
        found.push(Finding {
            code: Code::InputNotStarted,
            id: format!("liveness:input_not_started:{agent}:{event}"),
            agent: agent.clone(),
            agent_name: agent_name(&record),
            detail,
            subject: String::new(),
            event: Some(event.clone()),
            first_at: created,
            at: now,
        });
    }
    Ok(())
}

fn waiting_sessions(
    tx: &Transaction<'_>,
    now: f64,
    found: &mut Vec<Finding>,
) -> Result<(), StoreError> {
    let mut statement = tx.prepare_cached(
        "SELECT a.id,COALESCE(json_extract(a.record,'$.accountKey'),'default'),COALESCE(json_extract(a.record,'$.name'),'Agent'),\
         (SELECT MIN(e.created) FROM runtime_events e WHERE e.agent=a.id AND e.status='pending' AND e.epoch=json_extract(a.record,'$.epoch')) \
         FROM runtime_agents a WHERE a.deleted_at IS NULL AND a.status IN ('queued','starting') \
         AND json_extract(a.record,'$.startWait.reason')='provider_session' \
         AND json_extract(a.record,'$.startWait.epoch')=json_extract(a.record,'$.epoch') ORDER BY a.id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<f64>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut accounts = BTreeMap::<String, Vec<(String, String, f64)>>::new();
    for (agent, account, name, since) in rows {
        if let Some(since) = since.filter(|since| now - since >= SESSION_START_SECONDS) {
            accounts
                .entry(account)
                .or_default()
                .push((agent, name, since));
        }
    }
    for (account, waiting) in accounts {
        let Some((agent, name, _)) = waiting.iter().min_by(|a, b| a.2.total_cmp(&b.2)).cloned()
        else {
            continue;
        };
        let since = waiting.iter().map(|w| w.2).fold(f64::INFINITY, f64::min);
        let failure = session_start_failure(tx, &account)?;
        let chats = match waiting.len() {
            1 => "1 chat waits for it.".to_owned(),
            count => format!("{count} chats wait for it."),
        };
        let detail = match failure.as_ref().and_then(|value| value.reason.as_deref()) {
            Some(reason) => format!("{chats} The last start error: {reason}"),
            None => format!("{chats} Studio recorded no start error."),
        };
        found.push(Finding {
            code: Code::SessionNotStarted,
            id: format!("liveness:session_not_started:{account}:{}", since as i64),
            agent,
            agent_name: name,
            detail,
            subject: account,
            event: None,
            first_at: since,
            at: now,
        });
    }
    Ok(())
}

fn stalled_schedules(
    tx: &Transaction<'_>,
    now: f64,
    found: &mut Vec<Finding>,
) -> Result<(), StoreError> {
    let overdue = now - SCHEDULE_OVERDUE_SECONDS;
    // Usage resumes and capacity retries that the continuation scheduler owns:
    // only the row that the agent names counts, as the scheduler reads it.
    let mut statement = tx.prepare_cached(
        "SELECT 'usage',r.agent,r.record FROM runtime_usage_resumes r JOIN runtime_agents a ON a.id=r.agent \
         AND json_extract(a.record,'$.usageResume.id')=r.id \
         WHERE a.deleted_at IS NULL AND json_extract(r.record,'$.status')='scheduled' \
         AND json_extract(r.record,'$.dueAt')<?1 \
         UNION ALL SELECT 'capacity',r.agent,r.record FROM runtime_capacity_retries r JOIN runtime_agents a ON a.id=r.agent \
         AND json_extract(a.record,'$.capacityRetry.id')=r.id \
         WHERE a.deleted_at IS NULL AND ((json_extract(r.record,'$.status')='scheduled' AND json_extract(r.record,'$.dueAt')<?1) \
         OR (json_extract(r.record,'$.status')='starting' AND json_extract(r.record,'$.acceptedTurnId') IS NULL \
         AND json_extract(r.record,'$.claimedAt')<?1))",
    )?;
    let rows = statement
        .query_map([overdue], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (kind, agent, raw) in rows {
        let Ok(record) = serde_json::from_str::<ScheduleObservation>(&raw) else {
            continue;
        };
        let Some(owner) = merged_agent(tx, &agent)? else {
            continue;
        };
        let due = record.due_at.or(record.claimed_at).unwrap_or(now);
        let subject = if kind == "usage" {
            "Usage resume".to_owned()
        } else {
            format!(
                "Capacity retry, attempt {} of {}",
                record.attempt.unwrap_or(0),
                record.max_attempts.unwrap_or(0)
            )
        };
        let reason = record.reason.as_deref().filter(|reason| !reason.is_empty());
        found.push(Finding {
            code: Code::ScheduleStalled,
            id: format!(
                "liveness:schedule_stalled:{kind}:{}",
                record.id.as_deref().unwrap_or(&agent)
            ),
            agent: agent.clone(),
            agent_name: agent_name(&owner),
            detail: format!(
                "It is {} minutes past its due time and did not move.{}",
                minutes(now - due),
                reason
                    .map(|reason| format!(" Last reason: {reason}"))
                    .unwrap_or_default()
            ),
            subject,
            event: None,
            first_at: due,
            at: now,
        });
    }

    // Account transfers move only when a member wakes, so their clock is the
    // last change. A member in a short step, an idle member that waits for
    // release, or a member that waits for the owner's retry or cancel is stuck.
    let mut statement = tx.prepare_cached(
        "SELECT id,record FROM runtime_account_transfers WHERE json_extract(record,'$.status')='pending' \
         AND COALESCE(json_extract(record,'$.updated'),json_extract(record,'$.created'))<?1",
    )?;
    let rows = statement
        .query_map([overdue], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (id, raw) in rows {
        let Ok(transfer) = serde_json::from_str::<TransferObservation>(&raw) else {
            continue;
        };
        let mut stuck = Vec::new();
        for (member, state) in &transfer.members {
            let phase = state.phase.as_deref().unwrap_or_default();
            let waiting_idle = phase == "waiting"
                && merged_agent(tx, member)?.is_some_and(|record| {
                    record["inFlight"] != true
                        && !matches!(
                            record["status"].as_str(),
                            Some("running" | "starting" | "approval")
                        )
                });
            if matches!(
                phase,
                "reading"
                    | "interrupting"
                    | "submitted"
                    | "lazy_submitted"
                    | "ready"
                    | "blocked"
                    | "unknown"
            ) || waiting_idle
            {
                stuck.push(
                    match state.error.as_deref().filter(|error| !error.is_empty()) {
                        Some(error) => format!(
                            "{} ({phase}): {error}",
                            state.name.as_deref().unwrap_or(member)
                        ),
                        None => format!("{} ({phase})", state.name.as_deref().unwrap_or(member)),
                    },
                );
            }
        }
        if stuck.is_empty() {
            continue;
        }
        let lead = transfer.lead_id.unwrap_or_default();
        let name = merged_agent(tx, &lead)?
            .map(|record| agent_name(&record))
            .unwrap_or_else(|| "Agent".to_owned());
        let updated = transfer.updated.or(transfer.created).unwrap_or(now);
        found.push(Finding {
            code: Code::ScheduleStalled,
            id: format!("liveness:schedule_stalled:transfer:{id}:{}", updated as i64),
            agent: lead,
            agent_name: name,
            detail: format!(
                "It did not change for {} minutes. Waiting: {}. Retry or cancel the account transfer.",
                minutes(now - updated),
                stuck.join("; ")
            ),
            subject: format!(
                "Account transfer to {}",
                transfer.target.as_deref().unwrap_or("another account")
            ),
            event: None,
            first_at: updated,
            at: now,
        });
    }
    Ok(())
}

#[cfg(test)]
mod observation_tests {
    use super::*;

    #[test]
    fn observations_match_legacy_json_indexing() {
        for raw in [
            "null",
            "false",
            "9",
            "[]",
            "\"text\"",
            "{}",
            r#"{"id":null,"id":"last","dueAt":1,"claimedAt":1.5,"attempt":3.0,"maxAttempts":18446744073709551615,"reason":false}"#,
            r#"{"members":{"z":{"phase":"ready","name":null},"a":4,"z":{"phase":"blocked","error":"last"}},"created":-1,"updated":null}"#,
        ] {
            let old: Value = serde_json::from_str(raw).unwrap();
            let schedule: ScheduleObservation = serde_json::from_str(raw).unwrap();
            assert_eq!(schedule.id.as_deref(), old["id"].as_str());
            assert_eq!(schedule.due_at, old["dueAt"].as_f64());
            assert_eq!(schedule.claimed_at, old["claimedAt"].as_f64());
            assert_eq!(schedule.attempt, old["attempt"].as_u64());
            assert_eq!(schedule.max_attempts, old["maxAttempts"].as_u64());
            assert_eq!(schedule.reason.as_deref(), old["reason"].as_str());
            let session: SessionFailure = serde_json::from_str(raw).unwrap();
            assert_eq!(session.reason.as_deref(), old["reason"].as_str());
            let transfer: TransferObservation = serde_json::from_str(raw).unwrap();
            assert_eq!(transfer.lead_id.as_deref(), old["leadId"].as_str());
            assert_eq!(transfer.updated, old["updated"].as_f64());
            assert_eq!(transfer.created, old["created"].as_f64());
            let members = old["members"].as_object();
            assert_eq!(
                transfer.members.len(),
                members.map_or(0, |members| members.len())
            );
            for (id, member) in transfer.members {
                let expected = &old["members"][&id];
                assert_eq!(member.phase.as_deref(), expected["phase"].as_str());
                assert_eq!(member.name.as_deref(), expected["name"].as_str());
                assert_eq!(member.error.as_deref(), expected["error"].as_str());
            }
        }
        for raw in ["{", r#"{"future":1e400}"#, "[1e400]"] {
            assert_eq!(
                serde_json::from_str::<SessionFailure>(raw).is_ok(),
                serde_json::from_str::<Value>(raw).is_ok()
            );
        }
    }
}
