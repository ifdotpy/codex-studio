//! The liveness checker: invariants that find a stuck chat before its owner
//! does. Each finding is a row of the Studio errors list with a code and a
//! plain reason. Where it is safe, the checker runs one automatic recovery
//! through an existing idempotent path, and never sends an input:
//! - `turn_stalled`: a running chat whose provider sent nothing for
//!   [`SILENT_TURN_SECONDS`] while no tool runs. The account worker checks a
//!   Codex turn with native history and settles it once
//!   (`account_worker::silent_turns`). A Claude bridge holds its turn in
//!   memory and reports it active, so its row only names the stall.
//! - `input_not_started`: an owner message that waits longer than
//!   [`PENDING_INPUT_SECONDS`] while its chat is idle. The checker queues the
//!   chat once per message, as a new message would; the scheduler then claims
//!   only pending input. A held chat is not woken; its row says why.
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
use std::sync::Arc;
use studio_runtime_store::{
    AgentId, AgentRepository, EventWriteRepository, RuntimeStore, StoreError, Transaction,
};
use tokio::sync::{Notify, watch};

/// Unit: wall time between two liveness passes.
pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
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
/// Receipt-id prefix of the one wake for a waiting message.
const WAKE_PREFIX: &str = "liveness:wake:";
/// Receipt-id prefix of the latest native-history check of a silent turn.
const TURN_CHECK_PREFIX: &str = "liveness:turn-check:";
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
    /// The waiting message that one wake may start, when the chat is idle
    /// and not held.
    pub wake: Option<String>,
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

/// The outcome of the latest native-history check of a silent turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnCheck {
    /// Native history shows the turn still running.
    Active,
    /// Native history could not be read or compared.
    Unreadable,
}

/// The receipt id of the latest native-history check of `agent`'s turn.
pub(crate) fn turn_check_key(agent: &str) -> String {
    format!("{TURN_CHECK_PREFIX}{agent}")
}

/// Record the latest native-history check `check` of the silent turn `turn`.
pub fn record_turn_check(
    tx: &Transaction<'_>,
    agent: &str,
    turn: &str,
    check: &str,
    outcome: TurnCheck,
    now: f64,
) -> Result<(), StoreError> {
    EventWriteRepository::replace_receipt(
        tx,
        &turn_check_key(agent),
        RECEIPT_SIGNATURE,
        &json!({"turn":turn,"check":check,"outcome":match outcome {
            TurnCheck::Active => "active",
            TurnCheck::Unreadable => "unreadable",
        },"at":now})
        .to_string(),
    )?;
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
    Ok(found)
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
        let claude = record["provider"] == "claude";
        let check = EventWriteRepository::receipt(tx, &turn_check_key(&id))?
            .and_then(|receipt| serde_json::from_str::<Value>(&receipt.result).ok())
            .filter(|check| check["turn"].as_str() == Some(turn));
        let detail = if claude {
            format!(
                "The provider sent nothing for {} minutes, and no tool runs. The Claude bridge still holds the turn open, so Studio cannot settle it from native history.",
                minutes(now - since)
            )
        } else {
            match check.as_ref().and_then(|check| check["outcome"].as_str()) {
                // Native history shows the turn still running: not stuck.
                Some("active") => continue,
                Some("unreadable") => format!(
                    "The provider sent nothing for {} minutes, and no tool runs. Studio could not read the native history of the turn.",
                    minutes(now - since)
                ),
                // The account worker checks the turn within a minute; only an
                // unchecked turn past that grace is shown.
                _ if now - since < SILENT_TURN_SECONDS + CHECK_INTERVAL.as_secs_f64() * 2.0 => {
                    continue;
                }
                _ => format!(
                    "The provider sent nothing for {} minutes, and no tool runs. Studio has not checked the native history yet; the account session may not run.",
                    minutes(now - since)
                ),
            }
        };
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
            wake: None,
        });
    }
    Ok(())
}

/// Why a chat with a waiting message does not start by itself, or `None`
/// when one wake can start it.
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
        let woken = EventWriteRepository::receipt(tx, &format!("{WAKE_PREFIX}{event}"))?
            .and_then(|receipt| serde_json::from_str::<Value>(&receipt.result).ok())
            .and_then(|value| value["at"].as_f64());
        let waiting = if count > 1 {
            format!("{count} messages wait. ")
        } else {
            String::new()
        };
        let detail = match (&hold, woken) {
            (Some(hold), _) => format!("{waiting}{hold}"),
            (None, Some(at)) => format!(
                "{waiting}Studio woke the chat {} minutes ago, but it did not start.",
                minutes(now - at)
            ),
            (None, None) => format!("{waiting}The chat is idle. Studio wakes it once."),
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
            wake: (hold.is_none() && woken.is_none()).then_some(event),
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
            wake: None,
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
            wake: None,
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
            wake: None,
        });
    }
    Ok(())
}

/// What one pass changed.
#[derive(Debug, Default)]
pub struct PassReport {
    /// Chats queued once for a waiting message.
    pub woken: Vec<String>,
}

/// One liveness pass: wake each idle chat whose message waits, once per
/// message. The caller notifies the account registry when a chat was woken.
pub async fn pass(store: &RuntimeStore, now: f64) -> Result<PassReport, StoreError> {
    let targets = store
        .read_async("runtime-services/liveness", move |tx| {
            Ok::<_, StoreError>(
                findings_for_wake(tx, now)?
                    .into_iter()
                    .filter_map(|finding| Some((finding.agent, finding.wake?)))
                    .collect::<Vec<_>>(),
            )
        })
        .await?;
    let mut report = PassReport::default();
    for (agent, event) in targets {
        let woken = crate::scheduler::lifecycle_transaction_with(store, move |tx| {
            wake_once(tx, &agent, &event, now)
        })
        .await;
        if let Ok(Some(agent)) = woken {
            report.woken.push(agent);
        }
    }
    Ok(report)
}

fn findings_for_wake(tx: &Transaction<'_>, now: f64) -> Result<Vec<Finding>, StoreError> {
    let mut found = Vec::new();
    waiting_inputs(tx, now, &mut found)?;
    Ok(found)
}

/// Queue `agent` for its waiting message `event` once, after rechecking it in
/// this transaction. The receipt is written first, so a lost commit response
/// never wakes the same message twice.
fn wake_once(
    tx: &Transaction<'_>,
    agent: &str,
    event: &str,
    now: f64,
) -> Result<Option<String>, StoreError> {
    let still = findings_for_wake(tx, now)?
        .into_iter()
        .any(|finding| finding.agent == agent && finding.wake.as_deref() == Some(event));
    if !still {
        return Ok(None);
    }
    EventWriteRepository::put_receipt(
        tx,
        &format!("{WAKE_PREFIX}{event}"),
        RECEIPT_SIGNATURE,
        &json!({"agent":agent,"at":now}).to_string(),
    )?;
    let Some(record) = AgentRepository::get(tx, &AgentId::from(agent))? else {
        return Ok(None);
    };
    Ok(crate::event_write::queue_for_pending_input(tx, &record)
        .map_err(|_| StoreError::SchemaMismatch("liveness wake refused".into()))?
        .then(|| agent.to_owned()))
}

/// Run a pass every [`CHECK_INTERVAL`] until shutdown, and wake the account
/// registry after a pass that queued a chat.
pub async fn run(store: Arc<RuntimeStore>, wake: Arc<Notify>, mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        tokio::select! {
            () = tokio::time::sleep(CHECK_INTERVAL) => {}
            _ = shutdown.changed() => return,
        }
        let Ok(now) = crate::event_write::unix_time() else {
            continue;
        };
        match pass(&store, now).await {
            Ok(report) if !report.woken.is_empty() => wake.notify_one(),
            Ok(_) => {}
            Err(error) => {
                crate::diagnostics::log_stderr(format_args!("liveness pass failed: {error}"))
            }
        }
    }
}
