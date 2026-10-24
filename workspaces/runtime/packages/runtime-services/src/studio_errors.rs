//! The owner's list of agent work that ended because of a Studio error, not
//! because of the agent's own work: a command whose result Studio lost, input
//! that Studio did not send or could not confirm, and a turn that ended with
//! its provider connection. Each row names the reason in plain words and one
//! safe action. `resume` runs connection recovery, which reads native history
//! and never sends uncertain input again. `check` opens the agent, because
//! only the owner can decide. Studio never runs a command or sends input from
//! this list. A dismissed row stays hidden.
//!
//! A rule or a command can fail the same way many times, for example a rule
//! that checks every few minutes while its owner cannot run. Its failures of
//! one kind are one row with their count and time range. A failure that a
//! later run of the same rule followed with a success is not on the list.
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    BACKEND_RESTARTED_TURN_ERROR, MONITOR_OWNER_STOPPED_ERROR, PROVIDER_DISCONNECTED_TURN_ERROR,
    PROVIDER_RESUME_UNKNOWN_ERROR, PROVIDER_START_NOT_SENT_ERROR,
    PROVIDER_THREAD_START_UNKNOWN_ERROR, VM_PROVIDER_DISCONNECTED_TURN_ERROR,
};
use studio_runtime_store::{EventWriteRepository, StoreError, Transaction};

/// Oldest work the list reads. Unit: seconds.
pub const WINDOW_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
/// Pending input younger than this still waits for an ordinary start, not for
/// an owner. Unit: seconds.
pub const STUCK_INPUT_SECONDS: f64 = 10.0 * 60.0;
/// Most rows of one list.
pub const MAX_ROWS: usize = 200;
/// Receipt-id prefix of a dismissed row.
const DISMISSED_PREFIX: &str = "studio-errors:dismissed:";
/// Row-id prefix of the failures of one rule or command and kind. Dismissing
/// such a row hides its failures up to that time; a later one returns.
const MONITOR_GROUP_PREFIX: &str = "monitors:";
/// Row-id prefix of one monitor, as stored by dismissals before grouping.
const MONITOR_ROW_PREFIX: &str = "monitor:";
const DISMISSED_SIGNATURE: &str = "studio-error-dismissal";

/// Why a row is on the list; each kind has one reason text and one action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// A command whose result Studio lost; it may have run.
    MonitorOutcomeLost,
    /// A command that Studio did not run, or whose end Studio did not record.
    MonitorNotRecorded,
    /// Input that the provider proved it never received.
    InputNotSent,
    /// Busy-turn input that the agent did not confirm before its turn ended.
    InputUnconfirmed,
    /// Input whose delivery Studio could not prove either way.
    InputUncertain,
    /// Input that waits because of a Studio error.
    InputStuck,
    /// A turn that ended with its provider connection.
    TurnDisconnected,
    /// Studio could not prepare the agent's workspace.
    WorkspaceFailed,
    /// A running turn whose provider is silent while no tool runs.
    TurnStalled,
    /// An owner message that waits while its chat does not start.
    InputNotStarted,
    /// Chats that wait for an account session that does not start.
    SessionNotStarted,
    /// A usage resume, capacity retry or account transfer that did not move.
    ScheduleStalled,
}

impl Kind {
    pub fn code(self) -> &'static str {
        match self {
            Self::MonitorOutcomeLost => "monitor_outcome_lost",
            Self::MonitorNotRecorded => "monitor_not_recorded",
            Self::InputNotSent => "input_not_sent",
            Self::InputUnconfirmed => "input_unconfirmed",
            Self::InputUncertain => "input_uncertain",
            Self::InputStuck => "input_stuck",
            Self::TurnDisconnected => "turn_disconnected",
            Self::WorkspaceFailed => "workspace_failed",
            Self::TurnStalled => "turn_stalled",
            Self::InputNotStarted => "input_not_started",
            Self::SessionNotStarted => "session_not_started",
            Self::ScheduleStalled => "schedule_stalled",
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::MonitorOutcomeLost => {
                "Studio lost the result of this command. The command may have run. Studio does not run it again."
            }
            Self::MonitorNotRecorded => {
                "Studio did not run this command, or did not record its end."
            }
            Self::InputNotSent => "Studio did not send this message to the provider.",
            Self::InputUnconfirmed => {
                "The agent did not confirm this message. It may have read it. Studio does not send it again."
            }
            Self::InputUncertain => "Studio does not know if the provider received this message.",
            Self::InputStuck => "This message waits because of a Studio error.",
            Self::TurnDisconnected => "The provider connection ended during the turn.",
            Self::WorkspaceFailed => "Studio could not prepare the workspace of this agent.",
            Self::TurnStalled => {
                "The chat shows a running turn, but its provider sent nothing for 10 minutes."
            }
            Self::InputNotStarted => "A message waits, but its chat did not start.",
            Self::SessionNotStarted => "Chats wait for an account session that did not start.",
            Self::ScheduleStalled => "A scheduled step did not move after its due time.",
        }
    }

    /// The safe action: `resume` only where connection recovery decides from
    /// native history; `check` where only the owner can decide.
    pub fn action(self) -> &'static str {
        match self {
            Self::InputUncertain | Self::InputStuck | Self::TurnDisconnected => "resume",
            Self::MonitorOutcomeLost
            | Self::MonitorNotRecorded
            | Self::InputNotSent
            | Self::InputUnconfirmed
            | Self::WorkspaceFailed
            | Self::TurnStalled
            | Self::InputNotStarted
            | Self::SessionNotStarted
            | Self::ScheduleStalled => "check",
        }
    }

    /// The row kind of a liveness finding.
    fn of_liveness(code: crate::liveness::Code) -> Self {
        match code {
            crate::liveness::Code::TurnStalled => Self::TurnStalled,
            crate::liveness::Code::InputNotStarted => Self::InputNotStarted,
            crate::liveness::Code::SessionNotStarted => Self::SessionNotStarted,
            crate::liveness::Code::ScheduleStalled => Self::ScheduleStalled,
        }
    }
}

/// The kind of an agent's current error, when Studio caused it.
fn agent_kind(status: &str, error: &str) -> Option<Kind> {
    let disconnected = [
        PROVIDER_DISCONNECTED_TURN_ERROR,
        BACKEND_RESTARTED_TURN_ERROR,
        VM_PROVIDER_DISCONNECTED_TURN_ERROR,
        PROVIDER_RESUME_UNKNOWN_ERROR,
        PROVIDER_THREAD_START_UNKNOWN_ERROR,
    ];
    if matches!(status, "failed" | "interrupted") && disconnected.contains(&error) {
        return Some(Kind::TurnDisconnected);
    }
    if matches!(status, "failed" | "interrupted" | "paused")
        && (error.starts_with("WorkspaceRecoveryRequired")
            || error.starts_with("workspace-readiness/")
            || error.starts_with("The Linux VM helper is unavailable"))
    {
        return Some(Kind::WorkspaceFailed);
    }
    None
}

/// The kind of a monitor's end, when Studio caused it. A command with an exit
/// code ended on its own, and an owner's stop is the owner's choice.
fn monitor_kind(record: &Value) -> Option<Kind> {
    match record["status"].as_str() {
        Some("lost") => Some(Kind::MonitorOutcomeLost),
        Some("failed") if record["exitCode"].is_null() => {
            let error = record["error"].as_str().filter(|error| !error.is_empty())?;
            let owner_stopped = error == MONITOR_OWNER_STOPPED_ERROR
                || error.ends_with(&format!(": {MONITOR_OWNER_STOPPED_ERROR}"));
            (!owner_stopped).then_some(Kind::MonitorNotRecorded)
        }
        _ => None,
    }
}

/// The kind of an input event's state, when Studio caused it.
fn event_kind(status: &str, error: &str, age: f64) -> Option<Kind> {
    match status {
        "failed" if error == PROVIDER_START_NOT_SENT_ERROR => Some(Kind::InputNotSent),
        "failed" if error == studio_runtime_store::STEER_UNCONFIRMED_ERROR => {
            Some(Kind::InputUnconfirmed)
        }
        "uncertain" => Some(Kind::InputUncertain),
        "pending" if !error.is_empty() && age >= STUCK_INPUT_SECONDS => Some(Kind::InputStuck),
        _ => None,
    }
}

struct Agent {
    name: String,
    deleted: bool,
}

/// The current rows, newest first, without dismissed ones.
pub fn list(tx: &Transaction<'_>, now: f64) -> Result<Value, StoreError> {
    let since = now - WINDOW_SECONDS;
    let dismissed = dismissed(tx)?;
    let agent = |id: &str| -> Result<Option<Agent>, StoreError> {
        Ok(studio_runtime_store::AgentRepository::get(
            tx,
            &studio_runtime_store::AgentId::from(id),
        )?
        .map(|record| Agent {
            name: record
                .extra
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Agent")
                .to_owned(),
            deleted: studio_runtime_store::truthy(record.extra.get("deletedAt")),
        }))
    };
    let mut rows = Vec::new();
    let push = |rows: &mut Vec<Value>, row: Value| {
        if row["id"]
            .as_str()
            .is_some_and(|id| !dismissed.contains_key(id))
        {
            rows.push(row);
        }
    };

    let mut monitors = tx.prepare_cached(
        "SELECT id,record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('lost','failed') AND json_extract(record,'$.created')>=?1",
    )?;
    let found = monitors
        .query_map([since], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(monitors);
    let mut groups = std::collections::BTreeMap::<(String, &'static str), MonitorGroup>::new();
    for (id, raw) in found {
        let Ok(record) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(kind) = monitor_kind(&record) else {
            continue;
        };
        if dismissed.contains_key(&format!("{MONITOR_ROW_PREFIX}{id}")) {
            continue;
        }
        let agent_id = record["agent"].as_str().unwrap_or_default().to_owned();
        let command = record["command"].as_str().unwrap_or_default().to_owned();
        let rule = record["ruleId"]
            .as_str()
            .filter(|rule| !rule.is_empty())
            .map(str::to_owned);
        let key = match &rule {
            Some(rule) => format!("rule:{rule}"),
            None => format!(
                "command:{agent_id}:{}",
                &studio_runtime_store::sha256_hex(command.as_bytes())[..16]
            ),
        };
        let failure = MonitorFailure {
            id,
            created: record["created"].as_f64().unwrap_or(0.0),
            at: record["finished"]
                .as_f64()
                .or_else(|| record["created"].as_f64())
                .unwrap_or(0.0),
            error: record["error"].as_str().unwrap_or_default().to_owned(),
        };
        groups
            .entry((key, kind.code()))
            .or_insert_with(|| MonitorGroup {
                kind,
                agent: agent_id,
                rule,
                command: command.clone(),
                failures: Vec::new(),
            })
            .failures
            .push(failure);
    }
    for ((key, code), mut group) in groups {
        let row_id = format!("{MONITOR_GROUP_PREFIX}{key}:{code}");
        let dismissed_at = dismissed.get(&row_id).copied();
        let succeeded_at = match &group.rule {
            Some(rule) => {
                let earliest = group
                    .failures
                    .iter()
                    .map(|failure| failure.created)
                    .fold(f64::INFINITY, f64::min);
                last_rule_success(tx, rule, earliest)?
            }
            None => None,
        };
        group.failures.retain(|failure| {
            succeeded_at.is_none_or(|success| failure.created >= success)
                && dismissed_at.is_none_or(|dismissed| failure.at > dismissed)
        });
        group
            .failures
            .sort_by(|a, b| a.at.total_cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
        let (Some(first), Some(last)) = (group.failures.first(), group.failures.last()) else {
            continue;
        };
        let Some(owner) = agent(&group.agent)? else {
            continue;
        };
        if owner.deleted {
            continue;
        }
        rows.push(json!({
            "id":row_id,
            "kind":group.kind.code(),
            "reason":group.kind.reason(),
            "action":group.kind.action(),
            "agentId":group.agent,
            "agentName":owner.name,
            "monitorId":last.id,
            "ruleId":group.rule,
            "detail":last.error,
            "subject":group.command,
            "count":group.failures.len(),
            "firstAt":first.at,
            "at":last.at,
        }));
    }

    let mut events = tx.prepare_cached(
        "SELECT id,agent,status,COALESCE(error,''),created,text FROM runtime_events WHERE created>=?1 AND status IN ('failed','uncertain','pending')",
    )?;
    let found = events
        .query_map([since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(events);
    for (id, agent_id, status, error, created, text) in found {
        let Some(kind) = event_kind(&status, &error, now - created) else {
            continue;
        };
        let Some(owner) = agent(&agent_id)? else {
            continue;
        };
        if owner.deleted {
            continue;
        }
        push(
            &mut rows,
            json!({
                "id":format!("event:{id}"),
                "kind":kind.code(),
                "reason":kind.reason(),
                "action":kind.action(),
                "agentId":agent_id,
                "agentName":owner.name,
                "eventId":id,
                "detail":error,
                "subject":text.chars().take(200).collect::<String>(),
                "count":1,
                "firstAt":created,
                "at":created,
            }),
        );
    }

    let mut agents = tx.prepare_cached(
        "SELECT id,status,COALESCE(json_extract(record,'$.error'),''),COALESCE(json_extract(record,'$.name'),'Agent'),COALESCE(json_extract(record,'$.turnId'),json_extract(record,'$.lastCompletedTurn'),''),COALESCE(json_extract(record,'$.activity.at'),json_extract(record,'$.created'),0) FROM runtime_agents WHERE deleted_at IS NULL AND status IN ('failed','interrupted','paused') AND json_type(record,'$.error')='text'",
    )?;
    let found = agents
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, f64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(agents);
    for (id, status, error, name, turn, at) in found {
        let Some(kind) = agent_kind(&status, &error) else {
            continue;
        };
        push(
            &mut rows,
            json!({
                "id":format!("agent:{id}:{turn}"),
                "kind":kind.code(),
                "reason":kind.reason(),
                "action":kind.action(),
                "agentId":id,
                "agentName":name,
                "detail":error,
                "subject":"",
                "count":1,
                "firstAt":at,
                "at":at,
            }),
        );
    }

    for finding in crate::liveness::findings(tx, now)? {
        let kind = Kind::of_liveness(finding.code);
        push(
            &mut rows,
            json!({
                "id":finding.id,
                "kind":kind.code(),
                "reason":kind.reason(),
                "action":kind.action(),
                "agentId":finding.agent,
                "agentName":finding.agent_name,
                "eventId":finding.event,
                "detail":finding.detail,
                "subject":finding.subject,
                "count":1,
                "firstAt":finding.first_at,
                "at":finding.at,
            }),
        );
    }

    rows.sort_by(|a, b| {
        b["at"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["at"].as_f64().unwrap_or(0.0))
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    rows.truncate(MAX_ROWS);
    Ok(json!({"items":rows}))
}

/// One failed or lost run of a rule or command.
struct MonitorFailure {
    id: String,
    /// Unix seconds of its start.
    created: f64,
    /// Unix seconds of its end, or of its start when it has none.
    at: f64,
    error: String,
}

/// The failures of one rule or command and kind.
struct MonitorGroup {
    kind: Kind,
    agent: String,
    rule: Option<String>,
    command: String,
    failures: Vec<MonitorFailure>,
}

/// Unix seconds of the start of the latest successful run of `rule` that
/// started at or after `since`.
fn last_rule_success(
    tx: &Transaction<'_>,
    rule: &str,
    since: f64,
) -> Result<Option<f64>, StoreError> {
    Ok(tx.query_row(
        "SELECT MAX(json_extract(record,'$.created')) FROM runtime_monitors WHERE json_extract(record,'$.status')='completed' AND json_extract(record,'$.created')>=?1 AND json_extract(record,'$.ruleId')=?2",
        rusqlite::params![since, rule],
        |row| row.get::<_, Option<f64>>(0),
    )?)
}

/// Dismissed row ids, each with the Unix seconds of its latest dismissal.
fn dismissed(tx: &Transaction<'_>) -> Result<std::collections::BTreeMap<String, f64>, StoreError> {
    let mut statement = tx.prepare_cached(
        "SELECT substr(id,?3),COALESCE(json_extract(result,'$.at'),0) FROM runtime_operation_receipts WHERE id>=?1 AND id<?2",
    )?;
    let ids = statement
        .query_map(
            rusqlite::params![
                DISMISSED_PREFIX,
                format!("{DISMISSED_PREFIX}~"),
                i64::try_from(DISMISSED_PREFIX.len() + 1).unwrap_or(i64::MAX)
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        )?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// Hide row `id` from the list. Dismissing it again changes nothing, except
/// for the row of a rule or command: that hides its failures up to `now`.
pub fn dismiss(tx: &Transaction<'_>, id: &str, now: f64) -> Result<(), StoreError> {
    let key = format!("{DISMISSED_PREFIX}{id}");
    let value = json!({"at":now}).to_string();
    if id.starts_with(MONITOR_GROUP_PREFIX) {
        EventWriteRepository::replace_receipt(tx, &key, DISMISSED_SIGNATURE, &value)?;
    } else if EventWriteRepository::receipt(tx, &key)?.is_none() {
        EventWriteRepository::put_receipt(tx, &key, DISMISSED_SIGNATURE, &value)?;
    }
    Ok(())
}

/// Dismiss only currently listed errors. The caller owns the encompassing transaction.
/// Unknown and already dismissed ids have no effect, and duplicate ids are reported once.
pub fn dismiss_many(tx: &Transaction<'_>, ids: &[String], now: f64) -> Result<Value, StoreError> {
    let before = list(tx, now)?;
    let known: std::collections::BTreeSet<_> = before["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str())
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut dismissed_ids = Vec::new();
    let mut unknown_ids = Vec::new();
    for id in ids {
        if !seen.insert(id) {
            continue;
        }
        if known.contains(id.as_str()) {
            dismiss(tx, id, now)?;
            dismissed_ids.push(id);
        } else {
            unknown_ids.push(id);
        }
    }
    let mut result = list(tx, now)?;
    result["dismissedIds"] = json!(dismissed_ids);
    result["unknownIds"] = json!(unknown_ids);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_studio_cause_has_its_kind_and_an_agent_cause_has_none() {
        assert_eq!(
            monitor_kind(&json!({"status":"lost","exitCode":null})),
            Some(Kind::MonitorOutcomeLost)
        );
        assert_eq!(
            monitor_kind(
                &json!({"status":"failed","exitCode":null,"error":"Monitor changed during preparation"})
            ),
            Some(Kind::MonitorNotRecorded)
        );
        assert_eq!(
            monitor_kind(&json!({"status":"failed","exitCode":1,"error":""})),
            None,
            "an exit code is the command's own end"
        );
        assert_eq!(
            monitor_kind(&json!({"status":"failed","exitCode":null,
                "error":format!("The command was not started: {MONITOR_OWNER_STOPPED_ERROR}")})),
            None,
            "the owner stopped the turn"
        );
        assert_eq!(
            event_kind("failed", PROVIDER_START_NOT_SENT_ERROR, 0.0),
            Some(Kind::InputNotSent)
        );
        assert_eq!(
            event_kind("failed", studio_runtime_store::STEER_UNCONFIRMED_ERROR, 0.0),
            Some(Kind::InputUnconfirmed)
        );
        assert_eq!(event_kind("uncertain", "", 0.0), Some(Kind::InputUncertain));
        assert_eq!(
            event_kind(
                "pending",
                PROVIDER_RESUME_UNKNOWN_ERROR,
                STUCK_INPUT_SECONDS
            ),
            Some(Kind::InputStuck)
        );
        assert_eq!(
            event_kind("pending", PROVIDER_RESUME_UNKNOWN_ERROR, 1.0),
            None,
            "young input still waits for its start"
        );
        assert_eq!(event_kind("pending", "", STUCK_INPUT_SECONDS), None);
        assert_eq!(
            agent_kind("interrupted", PROVIDER_DISCONNECTED_TURN_ERROR),
            Some(Kind::TurnDisconnected)
        );
        assert_eq!(
            agent_kind("paused", "workspace-readiness/token-stale"),
            Some(Kind::WorkspaceFailed)
        );
        for error in [
            PROVIDER_RESUME_UNKNOWN_ERROR,
            PROVIDER_THREAD_START_UNKNOWN_ERROR,
        ] {
            assert_eq!(
                agent_kind("failed", error),
                Some(Kind::TurnDisconnected),
                "{error}"
            );
        }
        assert_eq!(agent_kind("paused", "Stopped by user"), None);
        assert_eq!(agent_kind("failed", "This content was flagged"), None);
    }

    #[test]
    fn only_history_decided_kinds_offer_resume() {
        for kind in [
            Kind::InputUncertain,
            Kind::InputStuck,
            Kind::TurnDisconnected,
        ] {
            assert_eq!(kind.action(), "resume", "{kind:?}");
        }
        for kind in [
            Kind::MonitorOutcomeLost,
            Kind::MonitorNotRecorded,
            Kind::InputNotSent,
            Kind::InputUnconfirmed,
            Kind::WorkspaceFailed,
            Kind::TurnStalled,
            Kind::InputNotStarted,
            Kind::SessionNotStarted,
            Kind::ScheduleStalled,
        ] {
            assert_eq!(kind.action(), "check", "{kind:?}");
        }
    }
}
