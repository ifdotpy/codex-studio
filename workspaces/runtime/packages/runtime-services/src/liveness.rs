//! The liveness checker: invariants that find a stuck chat before its owner
//! does. Each finding is a row of the Studio errors list with a code and a
//! plain reason. Detection only reads stored state. It never settles a turn,
//! wakes a chat, restarts a session, or sends a provider request:
//! - `turn_stalled`: a running chat whose provider sent nothing for
//!   [`SILENT_TURN_SECONDS`] while no tool runs.
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
use serde_json::{Value, json};
use std::collections::BTreeMap;
use studio_runtime_store::{
    AgentId, AgentRepository, EventWriteRepository, StoreError, Transaction,
};
/// Unit: seconds without a provider event before a running turn is stalled.
pub const SILENT_TURN_SECONDS: f64 = 600.0;
/// Unit: seconds an owner message waits in an idle chat before it is stuck.
pub const PENDING_INPUT_SECONDS: f64 = 120.0;
/// Unit: seconds chats wait for their account session before it is stuck.
pub const SESSION_START_SECONDS: f64 = 120.0;
/// Unit: seconds after its due time before a schedule is stuck.
pub const SCHEDULE_OVERDUE_SECONDS: f64 = 900.0;
/// Receipt-id prefix of the last start error of an account session.
const SESSION_START_FAILURE_PREFIX: &str = "liveness:session-start-failure:";
const RECEIPT_SIGNATURE: &str = "liveness";

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
    let key = format!("{SESSION_START_FAILURE_PREFIX}{account}");
    let current = EventWriteRepository::receipt(tx, &key)?
        .and_then(|receipt| serde_json::from_str::<Value>(&receipt.result).ok());
    if current
        .as_ref()
        .is_some_and(|value| value["reason"].as_str() == Some(reason))
    {
        return Ok(());
    }
    EventWriteRepository::replace_receipt(
        tx,
        &key,
        RECEIPT_SIGNATURE,
        &json!({"reason":reason,"at":now}).to_string(),
    )?;
    Ok(())
}

/// Forget the start error of `account` once its session runs.
pub fn clear_session_start_failure(tx: &Transaction<'_>, account: &str) -> Result<(), StoreError> {
    tx.prepare_cached("DELETE FROM runtime_operation_receipts WHERE id=?1 AND signature=?2")?
        .execute(rusqlite::params![
            format!("{SESSION_START_FAILURE_PREFIX}{account}"),
            RECEIPT_SIGNATURE
        ])?;
    Ok(())
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

/// Whether a running chat's provider is silent while no tool runs.
pub fn silent_turn(record: &Value, now: f64) -> Option<f64> {
    if record["status"] != "running"
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
        "SELECT id FROM runtime_agents WHERE deleted_at IS NULL AND status='running' AND COALESCE(in_flight,0)=1",
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
        let failure =
            EventWriteRepository::receipt(tx, &format!("{SESSION_START_FAILURE_PREFIX}{account}"))?
                .and_then(|receipt| serde_json::from_str::<Value>(&receipt.result).ok());
        let chats = match waiting.len() {
            1 => "1 chat waits for it.".to_owned(),
            count => format!("{count} chats wait for it."),
        };
        let detail = match failure.as_ref().and_then(|value| value["reason"].as_str()) {
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
        let Ok(record) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(owner) = merged_agent(tx, &agent)? else {
            continue;
        };
        let due = record["dueAt"]
            .as_f64()
            .or_else(|| record["claimedAt"].as_f64())
            .unwrap_or(now);
        let subject = if kind == "usage" {
            "Usage resume".to_owned()
        } else {
            format!(
                "Capacity retry, attempt {} of {}",
                record["attempt"].as_u64().unwrap_or(0),
                record["maxAttempts"].as_u64().unwrap_or(0)
            )
        };
        let reason = record["reason"]
            .as_str()
            .filter(|reason| !reason.is_empty());
        found.push(Finding {
            code: Code::ScheduleStalled,
            id: format!(
                "liveness:schedule_stalled:{kind}:{}",
                record["id"].as_str().unwrap_or(&agent)
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
        let Ok(transfer) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let mut stuck = Vec::new();
        for (member, state) in transfer["members"].as_object().into_iter().flatten() {
            let phase = state["phase"].as_str().unwrap_or_default();
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
                    match state["error"].as_str().filter(|error| !error.is_empty()) {
                        Some(error) => format!(
                            "{} ({phase}): {error}",
                            state["name"].as_str().unwrap_or(member)
                        ),
                        None => format!("{} ({phase})", state["name"].as_str().unwrap_or(member)),
                    },
                );
            }
        }
        if stuck.is_empty() {
            continue;
        }
        let lead = transfer["leadId"].as_str().unwrap_or_default().to_owned();
        let name = merged_agent(tx, &lead)?
            .map(|record| agent_name(&record))
            .unwrap_or_else(|| "Agent".to_owned());
        let updated = transfer["updated"]
            .as_f64()
            .or_else(|| transfer["created"].as_f64())
            .unwrap_or(now);
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
                transfer["targetAccountKey"].as_str().unwrap_or("another account")
            ),
            event: None,
            first_at: updated,
            at: now,
        });
    }
    Ok(())
}
