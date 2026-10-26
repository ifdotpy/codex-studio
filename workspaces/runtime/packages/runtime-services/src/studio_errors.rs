//! The owner's list of agent work that ended because of a Studio error, not
//! because of the agent's own work: a command whose result Studio lost, input
//! that Studio did not send or could not confirm, and a turn that ended with
//! its provider connection. Each row names the reason in plain words and one
//! safe action. `resume` runs connection recovery, which reads native history
//! and never sends uncertain input again. `check` opens the agent, because
//! only the owner can decide. Studio never runs a command or sends input from
//! this list. A dismissed row stays hidden.
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use studio_runtime_schema::shared_values::{
    BACKEND_RESTARTED_TURN_ERROR, LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
    MONITOR_OWNER_STOPPED_ERROR, PROVIDER_DISCONNECTED_TURN_ERROR, PROVIDER_RESUME_UNKNOWN_ERROR,
    PROVIDER_START_NOT_SENT_ERROR, VM_PROVIDER_DISCONNECTED_TURN_ERROR,
};
use studio_runtime_store::{EventWriteRepository, StoreError, StudioErrorSources, Transaction};

/// Oldest work the list reads. Unit: seconds.
pub const WINDOW_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
/// Pending input younger than this still waits for an ordinary start, not for
/// an owner. Unit: seconds.
pub const STUCK_INPUT_SECONDS: f64 = 10.0 * 60.0;
/// Most rows of one list.
pub const MAX_ROWS: usize = 200;
/// Receipt-id prefix of a dismissed row.
const DISMISSED_PREFIX: &str = "studio-errors:dismissed:";
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
            | Self::WorkspaceFailed => "check",
        }
    }
}

/// The kind of an agent's current error, when Studio caused it.
fn agent_kind(status: &str, error: &str) -> Option<Kind> {
    let disconnected = [
        PROVIDER_DISCONNECTED_TURN_ERROR,
        BACKEND_RESTARTED_TURN_ERROR,
        VM_PROVIDER_DISCONNECTED_TURN_ERROR,
        LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
        PROVIDER_RESUME_UNKNOWN_ERROR,
    ];
    if matches!(status, "failed" | "interrupted") && disconnected.contains(&error) {
        return Some(Kind::TurnDisconnected);
    }
    if matches!(status, "failed" | "interrupted" | "paused")
        && (error.starts_with("WorkspaceRecoveryRequired")
            || error.starts_with("workspace-readiness/")
            || error.starts_with("The VM helper is unavailable")
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

#[derive(Clone)]
struct Agent {
    name: String,
    deleted: bool,
}

/// The current rows, newest first, without dismissed ones.
pub fn list(tx: &Transaction<'_>, now: f64) -> Result<Value, StoreError> {
    Ok(build(tx, now)?.0)
}

/// What a full list build saw beyond its rows: the owners that hide rows
/// because they are missing or deleted, and the number of dismissals.
struct Baseline {
    hidden_owners: BTreeSet<String>,
    dismissed: usize,
}

/// The list and its baseline.
fn build(tx: &Transaction<'_>, now: f64) -> Result<(Value, Baseline), StoreError> {
    let since = now - WINDOW_SECONDS;
    let dismissed = dismissed(tx)?;
    let mut hidden_owners = BTreeSet::new();
    // Many rows share an owner; each owner record is read and decoded once.
    let mut owners = std::collections::HashMap::<String, Option<Agent>>::new();
    let mut agent = |id: &str| -> Result<Option<Agent>, StoreError> {
        if let Some(owner) = owners.get(id) {
            return Ok(owner.clone());
        }
        let owner = studio_runtime_store::AgentRepository::get(
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
        });
        owners.insert(id.to_owned(), owner.clone());
        Ok(owner)
    };
    let mut rows = Vec::new();
    let push = |rows: &mut Vec<Value>, row: Value| {
        if row["id"].as_str().is_some_and(|id| !dismissed.contains(id)) {
            rows.push(row);
        }
    };

    // The store read selects only rows that `monitor_kind` may accept.
    let found = studio_runtime_store::MonitorsRepository::studio_error_candidates(tx, since)?;
    for (id, raw) in found {
        let Ok(record) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(kind) = monitor_kind(&record) else {
            continue;
        };
        let agent_id = record["agent"].as_str().unwrap_or_default();
        let Some(owner) = agent(agent_id)?.filter(|owner| !owner.deleted) else {
            hidden_owners.insert(agent_id.to_owned());
            continue;
        };
        push(
            &mut rows,
            json!({
                "id":format!("monitor:{id}"),
                "kind":kind.code(),
                "reason":kind.reason(),
                "action":kind.action(),
                "agentId":agent_id,
                "agentName":owner.name,
                "monitorId":id,
                "detail":record["error"].as_str().unwrap_or_default(),
                "subject":record["command"].as_str().unwrap_or_default(),
                "at":record["finished"].as_f64().or_else(|| record["created"].as_f64()).unwrap_or(0.0),
            }),
        );
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
        let Some(owner) = agent(&agent_id)?.filter(|owner| !owner.deleted) else {
            hidden_owners.insert(agent_id);
            continue;
        };
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
                "at":at,
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
    let baseline = Baseline {
        hidden_owners,
        dismissed: dismissed.len(),
    };
    Ok((json!({"items":rows}), baseline))
}

/// Server-owned refresh cadence for time-derived membership (stuck input and
/// the error history window). Unit: time; clients never poll this collection.
const TIME_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Seed persisted errors on startup and refresh time-derived membership.
pub async fn refresh(store: &studio_runtime_store::RuntimeStore) -> Result<(), StoreError> {
    store.transaction(publish).await
}

/// Source writes publish immediately through DefaultPublisher. Time alone may
/// also change membership, so the same owner refreshes until server shutdown.
pub async fn run(
    store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    while !*shutdown.borrow() {
        tokio::select! {
            () = tokio::time::sleep(TIME_REFRESH_INTERVAL) => {},
            _ = shutdown.changed() => return,
        }
        if let Err(error) = refresh(&store).await {
            eprintln!("Studio error entity refresh failed: {error}");
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Full list rebuilds on this writer thread, for tests that prove a write
    /// did not rebuild the list.
    static FULL_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Full list rebuilds on the calling writer thread so far.
#[cfg(test)]
pub(crate) fn full_publications() -> u64 {
    FULL_PUBLICATIONS.with(std::cell::Cell::get)
}

/// Publish the current server-owned error rows as sync entities in the same
/// transaction as a source row or dismissal changes. Equal rows retain their
/// existing entity sequence, while removed rows publish tombstones.
pub fn publish(tx: &Transaction<'_>) -> Result<(), StoreError> {
    #[cfg(test)]
    FULL_PUBLICATIONS.with(|count| count.set(count.get() + 1));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
        .as_secs_f64();
    let (value, baseline) = build(tx, now)?;
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreError::SchemaMismatch("Studio error rows are invalid".into()))?;
    let mut current = std::collections::BTreeSet::new();
    for item in items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| StoreError::SchemaMismatch("Studio error id is missing".into()))?;
        studio_runtime_store::SyncDocumentsRepository::put_entity(
            tx,
            "studioError",
            id,
            item.clone(),
            false,
        )?;
        current.insert(id.to_owned());
    }
    let mut statement = tx.prepare_cached(
        "SELECT id FROM sync_entities WHERE collection='studioError' AND deleted=0",
    )?;
    let existing = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in existing {
        if !current.contains(&id) {
            studio_runtime_store::SyncDocumentsRepository::put_entity(
                tx,
                "studioError",
                &id,
                Value::Null,
                true,
            )?;
        }
    }
    BASELINE.with(|current| *current.borrow_mut() = Some(baseline));
    Ok(())
}

thread_local! {
    /// The baseline of the last full publication on this writer thread. A store
    /// runs all its writer transactions on one thread, so this is per store.
    /// `None` until the first full publication, which `entity_publication::install`
    /// runs at startup. A rollback after a publication can leave it newer than
    /// the committed state; then a missed change waits for the next refresh of
    /// `run`, at most `TIME_REFRESH_INTERVAL`.
    static BASELINE: std::cell::RefCell<Option<Baseline>> = const { std::cell::RefCell::new(None) };
}

/// Publish the list in the writer transaction that changed `sources`, unless
/// no changed row can change it. A full build reads every candidate row of
/// the window, so most writer transactions, which touch rows outside the list,
/// must skip it.
pub fn publish_changes(
    tx: &Transaction<'_>,
    sources: &StudioErrorSources,
) -> Result<(), StoreError> {
    if may_change(tx, sources)? {
        publish(tx)?;
    }
    Ok(())
}

/// Whether the changed `sources` can change the published list. The published
/// rows are the newest `MAX_ROWS` visible rows, so a row that is neither
/// published nor a visible candidate now cannot change them. A deleted row has
/// no id left; it matters only if a published row lost its source.
fn may_change(tx: &Transaction<'_>, sources: &StudioErrorSources) -> Result<bool, StoreError> {
    let Some(dismissed_count) = BASELINE.with(|baseline| {
        baseline
            .borrow()
            .as_ref()
            .map(|baseline| baseline.dismissed)
    }) else {
        return Ok(true);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
        .as_secs_f64();
    let since = now - WINDOW_SECONDS;
    if sources
        .receipts
        .iter()
        .any(|id| id.starts_with(DISMISSED_PREFIX))
        || (sources.receipts_removed && dismissed(tx)?.len() != dismissed_count)
    {
        return Ok(true);
    }
    if sources.rows_removed && published_source_missing(tx)? {
        return Ok(true);
    }
    for id in &sources.monitors {
        if is_published(tx, &format!("monitor:{id}"))? {
            return Ok(true);
        }
        let candidate =
            studio_runtime_store::MonitorsRepository::studio_error_candidate(tx, id, since)?;
        if candidate
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .is_some_and(|record| monitor_kind(&record).is_some())
        {
            return Ok(true);
        }
    }
    for id in &sources.events {
        if is_published(tx, &format!("event:{id}"))? {
            return Ok(true);
        }
        let event = tx
            .prepare_cached(
                "SELECT status,COALESCE(error,''),created FROM runtime_events WHERE id=?1 AND created>=?2",
            )?
            .query_row(rusqlite::params![id, since], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })
            .optional()?;
        if event.is_some_and(|(status, error, created)| {
            event_kind(&status, &error, now - created).is_some()
        }) {
            return Ok(true);
        }
    }
    for id in &sources.agents {
        if agent_may_change(tx, id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a write of agent `id` can change the list: through its own error
/// row, the owner name or visibility of its published rows, or the
/// visibility of rows it hid at the last full build.
fn agent_may_change(tx: &Transaction<'_>, id: &str) -> Result<bool, StoreError> {
    let own_prefix = format!("agent:{id}:");
    let own_published: bool = tx
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='studioError' AND deleted=0 AND id>=?1 AND id<?2)",
        )?
        .query_row(
            rusqlite::params![own_prefix, format!("agent:{id};")],
            |row| row.get(0),
        )?;
    if own_published {
        return Ok(true);
    }
    let own = tx
        .prepare_cached(
            "SELECT status,json_extract(record,'$.error') FROM runtime_agents WHERE id=?1 AND deleted_at IS NULL AND status IN ('failed','interrupted','paused') AND json_type(record,'$.error')='text'",
        )?
        .query_row([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?;
    if own.is_some_and(|(status, error)| agent_kind(&status, &error).is_some()) {
        return Ok(true);
    }
    let owner =
        studio_runtime_store::AgentRepository::get(tx, &studio_runtime_store::AgentId::from(id))?
            .filter(|record| !studio_runtime_store::truthy(record.extra.get("deletedAt")))
            .map(|record| {
                record
                    .extra
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Agent")
                    .to_owned()
            });
    let mut statement = tx.prepare_cached(
        "SELECT json_extract(payload,'$.value.agentName') FROM sync_entities WHERE collection='studioError' AND deleted=0 AND json_extract(payload,'$.value.agentId')=?1",
    )?;
    let names = statement
        .query_map([id], |row| row.get::<_, Option<String>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if names.iter().any(|name| name.as_deref() != owner.as_deref()) {
        return Ok(true);
    }
    Ok(owner.is_some()
        && BASELINE.with(|baseline| {
            baseline
                .borrow()
                .as_ref()
                .is_none_or(|baseline| baseline.hidden_owners.contains(id))
        }))
}

/// Whether row `id` of the list is published.
fn is_published(tx: &Transaction<'_>, id: &str) -> Result<bool, StoreError> {
    Ok(tx
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='studioError' AND id=?1 AND deleted=0)",
        )?
        .query_row([id], |row| row.get(0))?)
}

/// Whether a published row lost its monitor, event or owner row.
fn published_source_missing(tx: &Transaction<'_>) -> Result<bool, StoreError> {
    Ok(tx
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sync_entities e WHERE e.collection='studioError' AND e.deleted=0 AND (\
             (e.id LIKE 'monitor:%' AND NOT EXISTS(SELECT 1 FROM runtime_monitors m WHERE m.id=substr(e.id,9))) OR \
             (e.id LIKE 'event:%' AND NOT EXISTS(SELECT 1 FROM runtime_events v WHERE v.id=substr(e.id,7))) OR \
             NOT EXISTS(SELECT 1 FROM runtime_agents a WHERE a.id=json_extract(e.payload,'$.value.agentId'))))",
        )?
        .query_row([], |row| row.get(0))?)
}

fn dismissed(tx: &Transaction<'_>) -> Result<std::collections::BTreeSet<String>, StoreError> {
    let mut statement = tx.prepare_cached(
        "SELECT substr(id,?3) FROM runtime_operation_receipts WHERE id>=?1 AND id<?2",
    )?;
    let ids = statement
        .query_map(
            rusqlite::params![
                DISMISSED_PREFIX,
                format!("{DISMISSED_PREFIX}~"),
                i64::try_from(DISMISSED_PREFIX.len() + 1).unwrap_or(i64::MAX)
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// Hide row `id` from the list. Dismissing it again changes nothing.
pub fn dismiss(tx: &Transaction<'_>, id: &str, now: f64) -> Result<(), StoreError> {
    let key = format!("{DISMISSED_PREFIX}{id}");
    if EventWriteRepository::receipt(tx, &key)?.is_none() {
        EventWriteRepository::put_receipt(
            tx,
            &key,
            DISMISSED_SIGNATURE,
            &json!({"at":now}).to_string(),
        )?;
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
            VM_PROVIDER_DISCONNECTED_TURN_ERROR,
            LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
        ] {
            assert_eq!(
                agent_kind("interrupted", error),
                Some(Kind::TurnDisconnected)
            );
        }
        for error in [
            "The VM helper is unavailable",
            "The Linux VM helper is unavailable",
        ] {
            assert_eq!(agent_kind("failed", error), Some(Kind::WorkspaceFailed));
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
        ] {
            assert_eq!(kind.action(), "check", "{kind:?}");
        }
    }

    /// The monitor read selects rows in SQL so that the list no longer parses
    /// every command that ended with its own exit code. The list must stay
    /// identical to the former read of every lost or failed monitor in the
    /// window, and the SQL selection must hold every row `monitor_kind`
    /// accepts.
    #[tokio::test]
    async fn monitor_rows_equal_the_full_read_and_the_sql_selection_holds_every_kind() {
        let root = tempfile::tempdir().expect("state directory");
        let store = studio_runtime_store::RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        let now = 1_000_000_000.0;
        store
            .transaction(move |tx| {
                for (id, extra) in [
                    ("owner", json!({"name":"Fixture worker"})),
                    ("gone", json!({"name":"Deleted worker","deletedAt":now})),
                ] {
                    let mut agent = json!({"id":id,"rootId":id,"epoch":0,"status":"waiting",
                        "autoWake":true,"cwd":std::env::temp_dir().to_string_lossy(),
                        "accountKey":"default","provider":"codex"});
                    agent.as_object_mut().expect("object").extend(
                        extra.as_object().expect("object").clone(),
                    );
                    studio_runtime_store::AgentRepository::put(
                        tx,
                        &studio_runtime_store::AgentRecord::from_value(agent)?,
                    )?;
                }
                let stopped = format!("The command was not started: {MONITOR_OWNER_STOPPED_ERROR}");
                let old = now - WINDOW_SECONDS - 1.0;
                let monitors = [
                    ("lost", json!({"status":"lost","exitCode":null,"error":"lost result"})),
                    ("lost-no-error", json!({"status":"lost"})),
                    ("not-recorded", json!({"status":"failed","exitCode":null,"error":"Server restarted"})),
                    ("probe-no-session", json!({"status":"failed","exitCode":null,"error":"No live provider session","ruleId":"rule"})),
                    ("no-exit-key", json!({"status":"failed","error":"Callback barrier was not completed"})),
                    ("probe-exit", json!({"status":"failed","exitCode":1,"error":"","ruleId":"rule"})),
                    ("own-exit-error", json!({"status":"failed","exitCode":2,"error":"boom"})),
                    ("owner-stopped", json!({"status":"failed","exitCode":null,"error":MONITOR_OWNER_STOPPED_ERROR})),
                    ("owner-stopped-prefixed", json!({"status":"failed","exitCode":null,"error":stopped})),
                    ("empty-error", json!({"status":"failed","exitCode":null,"error":""})),
                    ("number-error", json!({"status":"failed","exitCode":null,"error":7})),
                    ("completed", json!({"status":"completed","exitCode":null,"error":"x"})),
                    ("deleted-owner", json!({"status":"lost","agent":"gone"})),
                    ("missing-owner", json!({"status":"lost","agent":"nobody"})),
                    ("old-lost", json!({"status":"lost","created":old})),
                ];
                for (id, fields) in monitors {
                    let mut record = json!({"id":id,"agent":"owner","command":format!("run {id}"),
                        "created":now - 5.0,"finished":now - 1.0});
                    record.as_object_mut().expect("object").extend(
                        fields.as_object().expect("object").clone(),
                    );
                    tx.execute(
                        "INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)",
                        rusqlite::params![id, record.to_string()],
                    )?;
                }

                let since = now - WINDOW_SECONDS;
                let selected = studio_runtime_store::MonitorsRepository::studio_error_candidates(
                    tx, since,
                )?
                .into_iter()
                .map(|(id, _)| id)
                .collect::<std::collections::BTreeSet<_>>();
                // The former read: every lost or failed monitor in the window.
                let mut statement = tx.prepare(
                    "SELECT id,record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('lost','failed') AND json_extract(record,'$.created')>=?1",
                )?;
                let full = statement
                    .query_map([since], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                let mut expected = std::collections::BTreeMap::new();
                for (id, raw) in full {
                    let record: Value = serde_json::from_str(&raw).expect("fixture JSON");
                    if let Some(kind) = monitor_kind(&record) {
                        assert!(selected.contains(&id), "SQL selection drops {id}");
                        if record["agent"] == "owner" {
                            expected.insert(format!("monitor:{id}"), (kind.code(), "Fixture worker"));
                        }
                    }
                }
                assert_eq!(
                    expected.keys().map(String::as_str).collect::<Vec<_>>(),
                    [
                        "monitor:lost",
                        "monitor:lost-no-error",
                        "monitor:no-exit-key",
                        "monitor:not-recorded",
                        "monitor:probe-no-session",
                    ]
                );
                assert!(!selected.contains("probe-exit") && !selected.contains("own-exit-error"));

                let listed = list(tx, now)?["items"]
                    .as_array()
                    .expect("items")
                    .iter()
                    .filter(|row| row["monitorId"].is_string())
                    .map(|row| {
                        let id = row["id"].as_str().expect("id").to_owned();
                        let monitor = row["monitorId"].as_str().expect("monitor id").to_owned();
                        assert_eq!(row["subject"], format!("run {monitor}"));
                        assert_eq!(row["at"], now - 1.0);
                        (id, (row["kind"].as_str().expect("kind"), row["agentName"].as_str().expect("name")))
                    })
                    .map(|(id, (kind, name))| (id, (kind.to_owned(), name.to_owned())))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let expected = expected
                    .into_iter()
                    .map(|(id, (kind, name))| (id, (kind.to_owned(), name.to_owned())))
                    .collect::<std::collections::BTreeMap<_, _>>();
                assert_eq!(listed, expected);
                Ok(())
            })
            .await
            .expect("fixture transaction");
    }
}
