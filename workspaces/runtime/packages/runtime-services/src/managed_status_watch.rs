//! Managed orchestration status and watch actions.
//!
//! The status projection follows `codex_efficiency.py::model_directory` and
//! stores the bounded revision history in `runtime_model_status`. Rule writes
//! mirror `codex_rules.py::RulesRuntime.rules`; filesystem observations are
//! prepared before a writer transaction and monitor cancellation IDs are
//! returned only after the rule transaction commits.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_schema::agent_limits_generated::{
    DEFAULT_GLOBAL_CONCURRENCY, DEFAULT_MAX_TEAM_AGENTS, DEFAULT_SUBAGENT_CONCURRENCY,
    MIN_GLOBAL_CONCURRENCY, MIN_SUBAGENT_CONCURRENCY,
};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, MonitorRecord, RuleRecord, RulesRepository,
    RuntimeStore, StoreError, Table, truthy,
};
use thiserror::Error;

#[cfg(test)]
#[allow(clippy::duplicate_mod)]
#[path = "../tests/support/prepared_python.rs"]
mod prepared_python_test_support;

const MODEL_STATUS_HISTORY: i64 = 8;
const FINISHED_PAGE_DEFAULT: u64 = 20;
const FINISHED_PAGE_MAX: u64 = 50;
const FILE_PATH_MAX_CHARS: usize = 4096;
const RULE_TEXT_MAX_CHARS: usize = 12_000;
const RULE_NAME_MAX_CHARS: usize = 120;
const RULE_COMMAND_MAX_CHARS: usize = 12_000;
const RULE_YEAR_SECONDS: f64 = 31_536_000.0;
const RULE_DEFAULT_INTERVAL_SECONDS: u64 = 60;
const RULE_DEFAULT_STALL_SECONDS: u64 = 1800;
const DEFAULT_DELEGATION_DEPTH: u64 = 1;

#[derive(Debug, Error)]
pub enum StatusError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("The caller identity changed")]
    Stale,
    #[error("Invalid status arguments")]
    InvalidArguments,
    #[error("The status cursor is invalid or the list changed")]
    InvalidCursor,
    #[error("The global concurrency setting is invalid")]
    InvalidConcurrency,
}

#[derive(Debug, Error)]
pub enum RulesServiceError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Engine(#[from] crate::rules_engine::RuleError),
    #[error("The caller identity changed")]
    Stale,
    #[error("Unknown rule action")]
    UnknownAction,
    #[error("Invalid rule action: {0}")]
    Invalid(&'static str),
    #[error("Invalid file-watch path")]
    FilePath,
}

fn status_to_rules(error: StatusError) -> RulesServiceError {
    match error {
        StatusError::Store(error) => RulesServiceError::Store(error),
        StatusError::Sqlite(error) => RulesServiceError::Sqlite(error),
        StatusError::Json(_) => RulesServiceError::Invalid("stored rule data"),
        StatusError::Stale => RulesServiceError::Stale,
        StatusError::InvalidArguments => RulesServiceError::Invalid("rule arguments"),
        StatusError::InvalidCursor => RulesServiceError::Invalid("rule cursor"),
        StatusError::InvalidConcurrency => RulesServiceError::Invalid("global concurrency"),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RulesActionResult {
    pub value: Value,
    pub cancel_monitor_ids: Vec<String>,
}

/// Filesystem work for rule actions. Implementations must be called only before
/// the store's write transaction, never from its closure.
pub trait RuleWatchFiles {
    fn resolve(&self, cwd: &str, path: &str) -> Result<String, RulesServiceError>;
    fn fingerprint(&self, path: &str) -> Result<Option<Value>, RulesServiceError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalRuleWatchFiles;

impl RuleWatchFiles for LocalRuleWatchFiles {
    fn resolve(&self, cwd: &str, path: &str) -> Result<String, RulesServiceError> {
        if path.chars().count() > FILE_PATH_MAX_CHARS {
            return Err(RulesServiceError::Invalid("a path"));
        }
        let expanded = expand_home(path);
        let supplied = PathBuf::from(expanded);
        let candidate = if supplied.is_absolute() {
            supplied
        } else {
            Path::new(cwd).join(supplied)
        };
        let resolved = crate::projects::resolve_nonstrict(&candidate)
            .map_err(|_| RulesServiceError::FilePath)?;
        Ok(resolved.to_string_lossy().into_owned())
    }

    fn fingerprint(&self, path: &str) -> Result<Option<Value>, RulesServiceError> {
        match std::fs::metadata(path) {
            #[cfg(unix)]
            Ok(metadata) => {
                use std::os::unix::fs::MetadataExt;
                let nanos = metadata
                    .mtime()
                    .checked_mul(1_000_000_000)
                    .and_then(|seconds| seconds.checked_add(metadata.mtime_nsec()))
                    .ok_or(RulesServiceError::FilePath)?;
                Ok(Some(json!([nanos, metadata.len(), metadata.ino()])))
            }
            #[cfg(not(unix))]
            Ok(_) => Err(RulesServiceError::FilePath),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(RulesServiceError::FilePath),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StatusPathInput {
    agent_id: String,
    cwd: String,
    reservation: bool,
}

#[derive(Clone, Debug)]
struct StatusPathSnapshot {
    inputs: Vec<StatusPathInput>,
    resolved: BTreeMap<String, String>,
}

/// Build a status snapshot and retain its revision in the same serialized
/// transaction, matching the Python callback's state point.
pub fn model_directory(
    store: &RuntimeStore,
    actor_id: &str,
    actor_epoch: u64,
    args: &Value,
    now: f64,
) -> Result<Value, StatusError> {
    if !args.is_object() {
        return Err(StatusError::InvalidArguments);
    }
    if let Some(include_finished) = args.get("include_finished")
        && !include_finished.is_boolean()
    {
        return Err(StatusError::InvalidArguments);
    }
    let (root_id, inputs) = store.read_transaction_with_error(|connection| {
        let actor = checked_actor(connection, actor_id, actor_epoch)?;
        let root_id = actor.root_id.0;
        let inputs = status_path_inputs(connection, &root_id)?;
        Ok::<_, StatusError>((root_id, inputs))
    })?;
    let resolved = inputs
        .iter()
        .map(|input| {
            crate::projects::resolve_nonstrict(Path::new(&input.cwd))
                .map(|path| (input.agent_id.clone(), path.to_string_lossy().into_owned()))
                .map_err(|_| StatusError::InvalidArguments)
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let paths = StatusPathSnapshot { inputs, resolved };
    store.transaction_with(|tx| {
        status_in_transaction(tx, actor_id, actor_epoch, &root_id, &paths, args, now)
    })
}

fn status_in_transaction(
    tx: &Transaction<'_>,
    actor_id: &str,
    actor_epoch: u64,
    root_id: &str,
    paths: &StatusPathSnapshot,
    args: &Value,
    now: f64,
) -> Result<Value, StatusError> {
    let actor = checked_actor(tx, actor_id, actor_epoch)?;
    if actor.root_id.0 != root_id || status_path_inputs(tx, root_id)? != paths.inputs {
        return Err(StatusError::Stale);
    }
    let team = team_agents(tx, root_id)?;
    let ids: BTreeSet<String> = team.iter().map(|agent| agent.id.0.clone()).collect();
    let root = team
        .iter()
        .find(|agent| agent.id.0 == root_id)
        .ok_or(StatusError::Stale)?;

    let mut active_records = team
        .iter()
        .filter(|agent| !is_terminal_agent(agent.status.as_str()))
        .map(agent_status_record)
        .collect::<Vec<_>>();
    let all_monitors = Table::<MonitorRecord>::new(tx).list()?;
    let team_monitors = all_monitors
        .into_iter()
        .filter(|monitor| {
            monitor
                .get("agent")
                .and_then(Value::as_str)
                .is_some_and(|agent| ids.contains(agent))
        })
        .collect::<Vec<_>>();
    active_records.extend(
        team_monitors
            .iter()
            .filter(|monitor| is_active_monitor(monitor))
            .map(monitor_status_record),
    );
    active_records.sort_by(|left, right| {
        status_record_kind(left)
            .cmp(status_record_kind(right))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });

    let finished_agents = team
        .iter()
        .filter(|agent| is_terminal_agent(agent.status.as_str()))
        .map(agent_status_record)
        .collect::<Vec<_>>();
    let mut finished_monitors = team_monitors
        .iter()
        .filter(|monitor| is_terminal_monitor(monitor))
        .map(monitor_status_record)
        .collect::<Vec<_>>();
    let mut finished_agents = finished_agents;
    finished_agents.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    finished_monitors.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));

    let mut counts = BTreeMap::<String, usize>::new();
    for agent in &team {
        *counts.entry(agent.status.as_str().to_owned()).or_default() += 1;
    }
    let finished_counts = json!({
        "agents": finished_agents.len(),
        "monitors": finished_monitors.len()
    });
    let defaults = worker_defaults(root);
    let capacity = capacity_snapshot(tx, &team, root, paths)?;
    let revision_source = json!([active_records, finished_counts, counts, defaults, capacity]);
    let revision = digest(&revision_source)?;

    let since_revision = args.get("since_revision").and_then(Value::as_str);
    let previous = if let Some(since_revision) = since_revision {
        tx.query_row(
            "SELECT record FROM runtime_model_status WHERE agent=?1 AND revision=?2",
            params![actor_id, since_revision],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    } else {
        None
    };
    let old_records = previous
        .as_deref()
        .map(serde_json::from_str::<Vec<Value>>)
        .transpose()
        .map_err(|_| {
            StatusError::Store(StoreError::InvalidStoredRecord {
                object: "runtime_model_status",
                id: actor_id.to_owned(),
            })
        })?
        .unwrap_or_default();
    let mut old = BTreeMap::<String, Value>::new();
    for record in old_records {
        let Some(key) = valid_status_key(&record) else {
            return Err(StatusError::Store(StoreError::InvalidStoredRecord {
                object: "runtime_model_status",
                id: actor_id.to_owned(),
            }));
        };
        if old.insert(key, record).is_some() {
            return Err(StatusError::Store(StoreError::InvalidStoredRecord {
                object: "runtime_model_status",
                id: actor_id.to_owned(),
            }));
        }
    }
    let current_records = active_records
        .iter()
        .filter_map(|record| Some((status_key(record)?, record.clone())))
        .collect::<BTreeMap<_, _>>();
    let changes = current_records
        .iter()
        .filter(|(key, value)| old.get(*key) != Some(*value))
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>();
    let removed = old
        .keys()
        .filter(|key| !current_records.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();

    tx.execute(
        "INSERT OR REPLACE INTO runtime_model_status(agent,revision,at,record) VALUES(?1,?2,?3,?4)",
        params![
            actor_id,
            revision,
            now,
            serde_json::to_string(&active_records)?
        ],
    )?;
    tx.execute(
        "DELETE FROM runtime_model_status WHERE agent=?1 AND revision NOT IN \
         (SELECT revision FROM runtime_model_status WHERE agent=?1 ORDER BY at DESC LIMIT ?2)",
        params![actor_id, MODEL_STATUS_HISTORY],
    )?;

    let mut result = json!({
        "apiVersion": 2,
        "revision": revision,
        "reset": previous.is_none(),
        "unchanged": previous.is_some() && since_revision == Some(revision.as_str()),
        "counts": counts,
        "changes": changes,
        "removed": removed,
        "finishedCounts": finished_counts,
        "workerDefaults": defaults,
        "capacity": capacity,
    });
    if args.get("include_finished") == Some(&Value::Bool(true)) {
        let limit = match args.get("limit") {
            None | Some(Value::Null) => FINISHED_PAGE_DEFAULT,
            Some(value) => value.as_u64().ok_or(StatusError::InvalidArguments)?,
        };
        if !(1..=FINISHED_PAGE_MAX).contains(&limit) {
            return Err(StatusError::InvalidArguments);
        }
        let offset = match args.get("cursor") {
            None | Some(Value::Null) => 0,
            Some(Value::String(value)) if value.is_empty() => 0,
            Some(Value::String(value)) => decode_cursor(value, &revision)?,
            _ => return Err(StatusError::InvalidCursor),
        };
        let total = finished_agents.len() + finished_monitors.len();
        if offset > total {
            return Err(StatusError::InvalidCursor);
        }
        let items = finished_agents
            .iter()
            .chain(finished_monitors.iter())
            .skip(offset)
            .take(limit as usize)
            .cloned()
            .collect::<Vec<_>>();
        let next_offset = offset + items.len();
        let next_cursor = (next_offset < total).then(|| encode_cursor(next_offset, &revision));
        result["finished"] = json!({
            "apiVersion": 2,
            "items": items,
            "total": total,
            "revision": revision,
            "nextCursor": next_cursor,
        });
    }
    Ok(result)
}

fn checked_actor(
    connection: &Connection,
    actor_id: &str,
    expected_epoch: u64,
) -> Result<AgentRecord, StatusError> {
    let actor =
        AgentRepository::get(connection, &AgentId::from(actor_id))?.ok_or(StatusError::Stale)?;
    if u64::try_from(actor.epoch).ok() != Some(expected_epoch)
        || truthy(actor.extra.get("deletedAt"))
    {
        return Err(StatusError::Stale);
    }
    Ok(actor)
}

fn team_agents(connection: &Connection, root: &str) -> Result<Vec<AgentRecord>, StatusError> {
    let mut statement = connection.prepare(
        "SELECT id FROM runtime_agents WHERE json_extract(record,'$.rootId')=?1 \
         AND (json_extract(record,'$.deletedAt') IS NULL OR json_extract(record,'$.deletedAt') IN (0,'')) \
         ORDER BY rowid",
    )?;
    let ids = statement
        .query_map([root], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .map(|id| {
            AgentRepository::get(connection, &AgentId::from(id.as_str()))?.ok_or_else(|| {
                StatusError::Store(StoreError::InvalidStoredRecord {
                    object: "runtime_agents",
                    id,
                })
            })
        })
        .collect()
}

fn worker_defaults(root: &AgentRecord) -> Value {
    let mut defaults = Map::from_iter([
        ("model".to_owned(), json!("gpt-6-luna")),
        ("effort".to_owned(), json!("high")),
        ("fastMode".to_owned(), Value::Bool(false)),
        ("daybreakEnabled".to_owned(), Value::Bool(false)),
        (
            "delegationDepth".to_owned(),
            json!(DEFAULT_DELEGATION_DEPTH),
        ),
    ]);
    if let Some(supplied) = root.extra.get("workerDefaults").and_then(Value::as_object) {
        defaults.extend(supplied.clone());
    }
    Value::Object(defaults)
}

fn capacity_snapshot(
    connection: &Connection,
    team: &[AgentRecord],
    root: &AgentRecord,
    paths: &StatusPathSnapshot,
) -> Result<Value, StatusError> {
    let slots = dispatch_active_slots(connection)?;
    let global_active = slots.len();
    let team_active = slots
        .iter()
        .filter(|slot| slot["rootId"] == root.id.0 && slot["id"] != root.id.0)
        .count();
    let global_limit = std::env::var("CODEX_CANVAS_CONCURRENCY")
        .map_or(Ok(DEFAULT_GLOBAL_CONCURRENCY), |value| {
            value
                .parse::<i64>()
                .map_err(|_| StatusError::InvalidConcurrency)
        })?
        .max(MIN_GLOBAL_CONCURRENCY);
    let reservations = paths
        .inputs
        .iter()
        .filter(|input| input.reservation)
        .filter_map(|input| {
            paths
                .resolved
                .get(&input.agent_id)
                .map(|path| (path.clone(), input.agent_id.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let root_concurrency = root
        .extra
        .get("concurrency")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| {
            if root.extra.get("agentMode") == Some(&json!("single")) {
                MIN_SUBAGENT_CONCURRENCY
            } else {
                DEFAULT_SUBAGENT_CONCURRENCY
            }
        });
    let root_max_agents = root
        .extra
        .get("maxAgents")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_MAX_TEAM_AGENTS);
    let active_statuses = ["running", "starting", "approval"];
    let mut queued = Vec::new();
    for agent in team
        .iter()
        .filter(|agent| agent.status.as_str() == "queued")
    {
        let record = agent.value();
        let id = agent.id.0.as_str();
        let mut reasons = Vec::new();
        if record.get("autoWake") != Some(&Value::Bool(true)) {
            reasons.push("paused");
        }
        for field in ["nativeFailureHold", "accountTransferId"] {
            if truthy(record.get(field)) {
                reasons.push(field);
            }
        }
        if crate::safety_buffering::active(&record).map_err(|_| StatusError::InvalidArguments)? {
            reasons.push("safety_retry");
        }
        if crate::sync_views::native_thread_block(&record).is_some() {
            reasons.push("native_thread_blocked");
        }
        let browser = record.get("browserRecovery").unwrap_or(&Value::Null);
        if browser.get("stage").and_then(Value::as_str) == Some("reconnecting")
            || (browser.get("stage").and_then(Value::as_str) == Some("pending")
                && truthy(browser.get("nativeRequest")))
        {
            reasons.push("browser_recovery");
        }
        let blocking_agent = paths
            .resolved
            .get(id)
            .and_then(|path| reservations.get(path));
        if blocking_agent.is_some() {
            reasons.push("workspace_operation");
        }
        if global_active >= global_limit as usize {
            reasons.push("global_concurrency");
        }
        if id != root.id.0 && team_active >= root_concurrency as usize {
            reasons.push("team_concurrency");
        }
        if reasons.is_empty() {
            reasons.push("awaiting_dispatch");
        }
        let mut row = json!({"id":id,"reasons":reasons});
        if let Some(blocker) = blocking_agent {
            row["blockingAgent"] = json!(blocker);
        }
        queued.push(row);
    }
    let team_active_all = team
        .iter()
        .filter(|agent| {
            truthy(agent.extra.get("inFlight")) || active_statuses.contains(&agent.status.as_str())
        })
        .count();
    Ok(json!({
        "teamLimit": root_concurrency,
        "globalLimit": global_limit,
        "occupied": team_active,
        "queuedCount": queued.len(),
        "teamActive": team_active_all,
        "globalActive": global_active,
        "maxAgents": root_max_agents,
        "queued": queued,
        "configure": {"userOnly":true,"minimum":0,"maximum":512},
    }))
}

fn dispatch_active_slots(connection: &Connection) -> Result<Vec<Value>, StatusError> {
    let mut statement = connection.prepare(
        "SELECT id,json_extract(record,'$.rootId') FROM runtime_agents \
         WHERE json_extract(record,'$.inFlight')=1 OR json_extract(record,'$.status') IN ('running','starting','approval') \
         UNION ALL SELECT id,json_extract(record,'$.rootId') FROM runtime_agents \
         WHERE json_extract(record,'$.startAttempt.activeAtReservation')=1 AND EXISTS (\
           SELECT 1 FROM json_each(runtime_agents.record,'$.startAttempt.events') AS input \
           JOIN runtime_events AS event ON event.id=input.value \
           WHERE event.agent=runtime_agents.id \
           AND event.epoch=json_extract(runtime_agents.record,'$.startAttempt.epoch') \
           AND event.status IN ('reserved','dispatching','uncertain'))",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut slots = BTreeMap::new();
    for (id, root) in rows {
        slots.insert(id.clone(), json!({"id":id,"rootId":root}));
    }
    Ok(slots.into_values().collect())
}

fn status_path_inputs(
    connection: &Connection,
    root_id: &str,
) -> Result<Vec<StatusPathInput>, StatusError> {
    let mut statement = connection.prepare(
        "SELECT id,json_extract(record,'$.cwd'), \
         COALESCE((json_type(record,'$.workspaceOperation')='text' AND json_extract(record,'$.workspaceOperation')!=''),0) \
         FROM runtime_agents WHERE \
         (json_extract(record,'$.deletedAt') IS NULL OR json_extract(record,'$.deletedAt') IN (0,'')) AND \
         ((json_type(record,'$.workspaceOperation')='text' AND json_extract(record,'$.workspaceOperation')!='') OR \
          (json_extract(record,'$.rootId')=?1 AND json_extract(record,'$.status')='queued')) ORDER BY id",
    )?;
    let rows = statement
        .query_map([root_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(agent_id, cwd, reservation)| {
            cwd.map(|cwd| StatusPathInput {
                agent_id,
                cwd,
                reservation,
            })
        })
        .collect())
}

fn agent_status_record(agent: &AgentRecord) -> Value {
    let mut record = json!({
        "id": agent.id.0,
        "name": agent.extra.get("name").cloned().unwrap_or(Value::Null),
        "status": agent.status.as_str(),
        "inFlight": agent.extra.get("inFlight").cloned().unwrap_or(Value::Null),
        "parentId": agent.extra.get("parentId").cloned().unwrap_or(Value::Null),
        "kind": "agent",
        "error": clip_value(agent.extra.get("error")),
    });
    if agent.status.as_str() == "parked" && truthy(agent.extra.get("parkedEvent")) {
        record["waitsForEvent"] = agent
            .extra
            .get("parkedEvent")
            .cloned()
            .unwrap_or(Value::Null);
    }
    record
}

fn monitor_status_record(monitor: &MonitorRecord) -> Value {
    json!({
        "id": monitor.get("id").cloned().unwrap_or(Value::Null),
        "agent": monitor.get("agent").cloned().unwrap_or(Value::Null),
        "status": monitor.get("status").cloned().unwrap_or(Value::Null),
        "exitCode": monitor.get("exitCode").cloned().unwrap_or(Value::Null),
        "kind": "monitor",
        "error": clip_value(monitor.get("error")),
    })
}

fn clip_value(value: Option<&Value>) -> Value {
    let raw = value
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_default();
    let mut bytes = raw.into_bytes();
    bytes.truncate(600);
    while std::str::from_utf8(&bytes).is_err() {
        bytes.pop();
    }
    Value::String(String::from_utf8(bytes).unwrap_or_default())
}

fn status_record_kind(record: &Value) -> &str {
    record
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn status_key(record: &Value) -> Option<String> {
    Some(format!(
        "{}:{}",
        record.get("kind")?.as_str()?,
        record.get("id")?.as_str()?
    ))
}

fn valid_status_key(record: &Value) -> Option<String> {
    let key = status_key(record)?;
    let nullable_string = |field: &str| {
        record
            .get(field)
            .is_some_and(|value| value.is_null() || value.is_string())
    };
    let valid = match record.get("kind")?.as_str()? {
        "agent" => {
            nullable_string("name")
                && record.get("status")?.is_string()
                && record
                    .get("inFlight")
                    .is_some_and(|value| value.is_null() || value.is_boolean())
                && nullable_string("parentId")
                && record.get("error")?.is_string()
                && record
                    .get("waitsForEvent")
                    .is_none_or(|value| value.is_string())
        }
        "monitor" => {
            nullable_string("agent")
                && record.get("status")?.is_string()
                && record
                    .get("exitCode")
                    .is_some_and(|value| value.is_null() || value.is_i64() || value.is_u64())
                && record.get("error")?.is_string()
        }
        _ => false,
    };
    if !valid || record.get("id")?.as_str()?.is_empty() {
        return None;
    }
    Some(key)
}

fn is_terminal_agent(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "interrupted")
}

fn is_active_monitor(monitor: &MonitorRecord) -> bool {
    matches!(
        monitor.get("status").and_then(Value::as_str),
        Some("running" | "starting" | "approval")
    )
}

fn is_terminal_monitor(monitor: &MonitorRecord) -> bool {
    matches!(
        monitor.get("status").and_then(Value::as_str),
        Some("completed" | "failed" | "cancelled" | "lost")
    )
}

fn digest(value: &Value) -> Result<String, StatusError> {
    let encoded = serde_json::to_vec(value)?;
    let hash = Sha256::digest(encoded);
    Ok(data_encoding::HEXLOWER.encode(&hash[..12]))
}

fn encode_cursor(offset: usize, revision: &str) -> String {
    URL_SAFE.encode(json!({"offset":offset,"revision":revision}).to_string())
}

fn decode_cursor(cursor: &str, revision: &str) -> Result<usize, StatusError> {
    let decoded = URL_SAFE
        .decode(cursor)
        .map_err(|_| StatusError::InvalidCursor)?;
    let value: Value = serde_json::from_slice(&decoded).map_err(|_| StatusError::InvalidCursor)?;
    let offset = value
        .get("offset")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(StatusError::InvalidCursor)?;
    if value.get("revision").and_then(Value::as_str) != Some(revision) {
        return Err(StatusError::InvalidCursor);
    }
    Ok(offset)
}

fn expand_home(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        return format!("{}{}", PathBuf::from(home).display(), &path[1..]);
    }
    path.to_owned()
}

fn stable_rule_id(request_id: &str) -> Result<String, RulesServiceError> {
    if request_id.is_empty() || request_id.chars().count() > 200 {
        return Err(RulesServiceError::Invalid("request id"));
    }
    Ok(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, request_id.as_bytes()).to_string())
}

fn rules_request_signature(
    actor_id: &str,
    actor_epoch: u64,
    data: Option<&Value>,
) -> Result<String, RulesServiceError> {
    let content = serde_json::to_vec(&json!({
        "actor": actor_id,
        "epoch": actor_epoch,
        "data": data,
    }))?;
    Ok(data_encoding::HEXLOWER.encode(&Sha256::digest(content)))
}

fn read_rules_receipt(
    connection: &Connection,
    receipt_id: &str,
    signature: &str,
) -> Result<Option<RulesActionResult>, RulesServiceError> {
    let Some((saved_signature, raw_result)) = connection
        .query_row(
            "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
            [receipt_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    if saved_signature != signature {
        return Err(RulesServiceError::Invalid(
            "Request identity was reused with different content",
        ));
    }
    let invalid_receipt = || {
        RulesServiceError::Store(StoreError::InvalidStoredRecord {
            object: "runtime_operation_receipts",
            id: receipt_id.to_owned(),
        })
    };
    let result: Value = serde_json::from_str(&raw_result).map_err(|_| invalid_receipt())?;
    let value = result.get("value").cloned().ok_or_else(invalid_receipt)?;
    let cancel_monitor_ids = result
        .get("cancelMonitorIds")
        .and_then(Value::as_array)
        .ok_or_else(invalid_receipt)?
        .iter()
        .map(|item| item.as_str().map(str::to_owned).ok_or_else(invalid_receipt))
        .collect::<Result<Vec<_>, _>>()?;
    if cancel_monitor_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid_receipt());
    }
    Ok(Some(RulesActionResult {
        value,
        cancel_monitor_ids,
    }))
}

/// Apply one `RulesRuntime.rules` action. The optional result's cancellation
/// IDs are sorted and must be delivered by the callback owner after this call
/// returns, which proves the rule transaction has committed.
pub fn rules(
    store: &RuntimeStore,
    actor_id: &str,
    actor_epoch: u64,
    data: Option<&Value>,
    request_id: &str,
    now: f64,
    files: &impl RuleWatchFiles,
) -> Result<RulesActionResult, RulesServiceError> {
    if !now.is_finite() {
        return Err(RulesServiceError::Invalid("time"));
    }
    let action = data
        .and_then(|value| value.get("action"))
        .and_then(Value::as_str)
        .unwrap_or(if data.is_none() { "list" } else { "save" });
    if data.is_some_and(|value| !value.is_object()) {
        return Err(RulesServiceError::Invalid("rule"));
    }
    if action == "list" || data.is_none() {
        let value = store.read_transaction_with_error(|connection| {
            let actor =
                checked_actor(connection, actor_id, actor_epoch).map_err(status_to_rules)?;
            let root = actor.root_id.0;
            let (rules, agents) = RulesRepository::list(connection)?;
            let live = agents
                .into_iter()
                .map(|agent| (agent.id.0, !truthy(agent.extra.get("deletedAt"))))
                .collect::<BTreeMap<_, _>>();
            let visible = rules
                .into_iter()
                .filter(|rule| {
                    rule.get("rootId").and_then(Value::as_str) == Some(root.as_str())
                        && rule
                            .get("agent")
                            .and_then(Value::as_str)
                            .and_then(|id| live.get(id))
                            .copied()
                            .unwrap_or(false)
                })
                .map(|rule| rule.value().clone())
                .collect::<Vec<_>>();
            Ok::<_, RulesServiceError>(json!({"rules": visible}))
        })?;
        return Ok(RulesActionResult {
            value,
            cancel_monitor_ids: Vec::new(),
        });
    }

    let body = data.ok_or(RulesServiceError::Invalid("rule"))?;
    if request_id.is_empty() || request_id.chars().count() > 200 {
        return Err(RulesServiceError::Invalid("request id"));
    }
    let receipt_id = format!("managed-rules:{actor_id}:{request_id}");
    let signature = rules_request_signature(actor_id, actor_epoch, data)?;
    let key = match body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        Some(id) => id.to_owned(),
        None => stable_rule_id(request_id)?,
    };
    let (prepared_actor, old_before, saved_receipt) =
        store.read_transaction_with_error(|connection| {
            let actor =
                checked_actor(connection, actor_id, actor_epoch).map_err(status_to_rules)?;
            let saved_receipt = read_rules_receipt(connection, &receipt_id, &signature)?;
            let old = RulesRepository::get(connection, &key)?.map(|record| record.value().clone());
            Ok::<_, RulesServiceError>((actor, old, saved_receipt))
        })?;
    if let Some(saved_receipt) = saved_receipt {
        return Ok(saved_receipt);
    }
    if let Some(old) = &old_before
        && old.get("agent").and_then(Value::as_str) != Some(actor_id)
    {
        return Err(RulesServiceError::Invalid(
            "This rule belongs to another agent",
        ));
    }

    let kind = body
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("interval");
    let prepared_path = if action == "save" && kind == "file" {
        let raw = text(body.get("path"), "a path", FILE_PATH_MAX_CHARS, false)?;
        Some(files.resolve(cwd(&prepared_actor)?, &raw)?)
    } else if action == "resume"
        && old_before
            .as_ref()
            .and_then(|old| old.get("kind"))
            .and_then(Value::as_str)
            == Some("file")
    {
        old_before
            .as_ref()
            .and_then(|old| old.get("path"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else {
        None
    };
    let fingerprint = match prepared_path.as_deref() {
        Some(path) => files.fingerprint(path)?,
        None => None,
    };

    let result = store.transaction_with(|tx| {
        let actor = checked_actor(tx, actor_id, actor_epoch).map_err(status_to_rules)?;
        if let Some(saved_receipt) = read_rules_receipt(tx, &receipt_id, &signature)? {
            return Ok::<_, RulesServiceError>(saved_receipt);
        }
        if actor.value() != prepared_actor.value() {
            return Err(RulesServiceError::Stale);
        }
        let old = RulesRepository::get(tx, &key)?.map(|record| record.value().clone());
        if old != old_before {
            return Err(RulesServiceError::Invalid(
                "The rule changed while it was being prepared",
            ));
        }
        if let Some(old) = &old
            && old.get("agent").and_then(Value::as_str) != Some(actor_id)
        {
            return Err(RulesServiceError::Invalid(
                "This rule belongs to another agent",
            ));
        }
        let mut cancel_monitor_ids = if matches!(action, "pause" | "delete") {
            active_rule_monitors(tx, &key)?
        } else {
            Vec::new()
        };
        let value = match action {
            "pause" | "resume" | "delete" => {
                let mut old = old.ok_or(RulesServiceError::Invalid("Unknown rule"))?;
                if action == "resume" && old.get("inFlight").and_then(Value::as_bool) == Some(true)
                {
                    return Err(RulesServiceError::Invalid(
                        "Wait for the current rule check before resuming",
                    ));
                }
                if action == "delete" {
                    RulesRepository::delete(tx, &key)?;
                    json!({"deleted": key})
                } else {
                    if action == "resume" && actor.extra.get("autoWake") != Some(&Value::Bool(true))
                    {
                        return Err(RulesServiceError::Invalid(
                            "Resume the agent before its rules",
                        ));
                    }
                    if let Some(object) = old.as_object_mut() {
                        object.remove("restartCheck");
                    }
                    let desired = if action == "resume" {
                        "active"
                    } else {
                        "paused"
                    };
                    let unchanged_low_workers = old.get("kind").and_then(Value::as_str)
                        == Some("low_workers")
                        && old.get("status").and_then(Value::as_str) == Some(desired)
                        && old.get("epoch") == Some(&json!(actor.epoch));
                    if !unchanged_low_workers {
                        if old.get("kind").and_then(Value::as_str) == Some("low_workers") {
                            old["lowSince"] = Value::Null;
                            old["alerted"] = Value::Bool(false);
                        }
                        if action == "resume"
                            && old.get("kind").and_then(Value::as_str) == Some("file")
                        {
                            let generation = old
                                .get("fileGeneration")
                                .and_then(Value::as_u64)
                                .unwrap_or(0)
                                .saturating_add(1);
                            old["fileActivityAt"] = json!(now);
                            old["fingerprint"] = fingerprint.clone().unwrap_or(Value::Null);
                            old["fileGeneration"] = json!(generation);
                            old["stallWakeGeneration"] = json!(generation as i64 - 1);
                        }
                        old["status"] = json!(desired);
                        old["epoch"] = json!(actor.epoch);
                        let interval = old
                            .get("intervalSeconds")
                            .and_then(Value::as_f64)
                            .unwrap_or(10.0);
                        old["nextAt"] = json!(now + interval);
                        old["error"] = Value::Null;
                        let stored = checked_rule(old.clone(), actor.value())?;
                        RulesRepository::put(tx, &stored)?;
                    }
                    old
                }
            }
            "save" => save_rule(
                tx,
                &actor,
                RuleSave {
                    body,
                    old: old.as_ref(),
                    key: &key,
                    kind,
                    now,
                    prepared_path: prepared_path.as_deref(),
                    fingerprint: fingerprint.as_ref(),
                },
            )?,
            _ => return Err(RulesServiceError::UnknownAction),
        };
        cancel_monitor_ids.sort();
        cancel_monitor_ids.dedup();
        let saved = RulesActionResult {
            value,
            cancel_monitor_ids,
        };
        let result = json!({
            "value": saved.value.clone(),
            "cancelMonitorIds": saved.cancel_monitor_ids.clone(),
        });
        tx.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3)",
            params![receipt_id, signature, result.to_string()],
        )?;
        Ok(saved)
    })?;
    Ok(result)
}

struct RuleSave<'a> {
    body: &'a Value,
    old: Option<&'a Value>,
    key: &'a str,
    kind: &'a str,
    now: f64,
    prepared_path: Option<&'a str>,
    fingerprint: Option<&'a Value>,
}

fn save_rule(
    tx: &Transaction<'_>,
    actor: &AgentRecord,
    save: RuleSave<'_>,
) -> Result<Value, RulesServiceError> {
    let RuleSave {
        body,
        old,
        key,
        kind,
        now,
        prepared_path,
        fingerprint,
    } = save;
    if old.is_some_and(|record| record.get("inFlight").and_then(Value::as_bool) == Some(true)) {
        return Err(RulesServiceError::Invalid(
            "Pause the rule and wait for its current check before editing",
        ));
    }
    if !matches!(kind, "interval" | "once" | "file" | "event" | "low_workers") {
        return Err(RulesServiceError::Invalid(
            "Choose interval, once, file, event, or low_workers",
        ));
    }
    let minimum = integer(body.get("minimumWorkers"), 8)?;
    let duration = integer(body.get("durationMinutes"), 30)?;
    if kind == "low_workers" {
        if actor.extra.get("isLead") != Some(&Value::Bool(true)) {
            return Err(RulesServiceError::Invalid(
                "Only an orchestrator can own a low-worker alert",
            ));
        }
        if !(1..=255).contains(&minimum) {
            return Err(RulesServiceError::Invalid(
                "Minimum subagents must be 1 to 255",
            ));
        }
        if !(1..=525_600).contains(&duration) {
            return Err(RulesServiceError::Invalid(
                "Duration must be 1 to 525600 minutes",
            ));
        }
        if !empty_text(body.get("command"), RULE_COMMAND_MAX_CHARS)?.is_empty() {
            return Err(RulesServiceError::Invalid(
                "A low-worker alert does not run a command",
            ));
        }
    }
    let interval = integer(
        body.get("intervalSeconds"),
        RULE_DEFAULT_INTERVAL_SECONDS as i64,
    )?;
    if !(10..=31_536_000).contains(&interval) {
        return Err(RulesServiceError::Invalid(
            "Interval must be 10 seconds to one year",
        ));
    }
    let stall_value = body
        .get("stallTimeoutSeconds")
        .or_else(|| body.get("stall_timeout_seconds"));
    let stall = integer(stall_value, RULE_DEFAULT_STALL_SECONDS as i64)?;
    if !(0..=31_536_000).contains(&stall) {
        return Err(RulesServiceError::Invalid(
            "stall_timeout_seconds must be 0 to 31536000",
        ));
    }
    let at = body
        .get("at")
        .filter(|value| !value.is_null())
        .and_then(Value::as_f64)
        .unwrap_or(now + interval as f64);
    if !at.is_finite() || at < now - 1.0 || at > now + RULE_YEAR_SECONDS {
        return Err(RulesServiceError::Invalid(
            "Choose a date within the next year",
        ));
    }
    let event = body
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("worker_completed");
    if !matches!(
        event,
        "worker_completed" | "monitor_exit" | "work_review" | "complaint"
    ) {
        return Err(RulesServiceError::Invalid("Unknown event type"));
    }
    let command = empty_text(body.get("command"), RULE_COMMAND_MAX_CHARS)?;
    let liveness_command = empty_text(
        body.get("livenessCommand")
            .or_else(|| body.get("liveness_command")),
        RULE_COMMAND_MAX_CHARS,
    )?;
    if !liveness_command.is_empty() && kind != "file" {
        return Err(RulesServiceError::Invalid(
            "liveness_command is only supported for file watches",
        ));
    }
    let name = text(body.get("name"), "a rule name", RULE_NAME_MAX_CHARS, false)?;
    let text_value = body
        .get("text")
        .filter(|value| !value.is_null())
        .map(|value| text(Some(value), "an instruction", RULE_TEXT_MAX_CHARS, false))
        .transpose()?
        .unwrap_or_else(|| "Check this event and continue the original task.".to_owned());
    let prior = old.and_then(Value::as_object);
    let mut record = json!({
        "id": key,
        "agent": actor.id.0,
        "rootId": actor.root_id.0,
        "epoch": actor.epoch,
        "name": name,
        "kind": kind,
        "intervalSeconds": interval,
        "nextAt": at,
        "at": at,
        "path": if kind == "file" { prepared_path.map(Value::from).unwrap_or(Value::Null) } else { Value::Null },
        "event": event,
        "command": command,
        "stallTimeoutSeconds": if kind == "file" { stall } else { 0 },
        "livenessCommand": liveness_command,
        "fileActivityAt": prior.and_then(|value| value.get("fileActivityAt")).cloned().unwrap_or(json!(now)),
        "fileGeneration": prior.and_then(|value| value.get("fileGeneration")).cloned().unwrap_or(json!(0)),
        "stallWakeGeneration": prior.and_then(|value| value.get("stallWakeGeneration")).cloned().unwrap_or(json!(-1)),
        "text": text_value,
        "status": if actor.extra.get("autoWake") == Some(&Value::Bool(true)) { "active" } else { "paused" },
        "created": prior.and_then(|value| value.get("created")).cloned().unwrap_or(json!(now)),
        "inFlight": false,
        "checks": prior.and_then(|value| value.get("checks")).cloned().unwrap_or(json!(0)),
        "wakes": prior.and_then(|value| value.get("wakes")).cloned().unwrap_or(json!(0)),
        "fingerprint": if kind == "file" { fingerprint.cloned().unwrap_or(Value::Null) } else { Value::Null },
    });
    if kind == "low_workers" {
        record["minimumWorkers"] = body.get("minimumWorkers").cloned().unwrap_or(json!(8));
        record["durationMinutes"] = body.get("durationMinutes").cloned().unwrap_or(json!(30));
        record["lowSince"] = Value::Null;
        record["alerted"] = Value::Bool(false);
        record["nextAt"] = json!(now);
    }
    if kind == "low_workers"
        && old.is_some_and(|old| {
            [
                "kind",
                "name",
                "text",
                "minimumWorkers",
                "durationMinutes",
                "epoch",
            ]
            .into_iter()
            .all(|field| old.get(field) == record.get(field))
        })
    {
        return Ok(old.cloned().unwrap_or(Value::Null));
    }
    let stored = checked_rule(record.clone(), actor.value())?;
    RulesRepository::put(tx, &stored)?;
    Ok(record)
}

fn checked_rule(record: Value, actor: Value) -> Result<RuleRecord, RulesServiceError> {
    let rule = RuleRecord::from_value(record).map_err(|_| RulesServiceError::Invalid("rule"))?;
    crate::rules_engine::RuleSnapshot::from_records(rule.value().clone(), actor)?;
    Ok(rule)
}

fn active_rule_monitors(
    connection: &Connection,
    rule_id: &str,
) -> Result<Vec<String>, RulesServiceError> {
    let mut statement = connection.prepare(
        "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.ruleId')=?1 \
         AND json_extract(record,'$.status') IN ('running','starting','approval') ORDER BY id",
    )?;
    Ok(statement
        .query_map([rule_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

fn cwd(agent: &AgentRecord) -> Result<&str, RulesServiceError> {
    agent
        .extra
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(RulesServiceError::Invalid("workspace path"))
}

fn integer(value: Option<&Value>, default: i64) -> Result<i64, RulesServiceError> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_i64().ok_or(RulesServiceError::Invalid("integer")),
    }
}

fn text(
    value: Option<&Value>,
    label: &'static str,
    maximum: usize,
    empty: bool,
) -> Result<String, RulesServiceError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or(RulesServiceError::Invalid(label))?
        .trim();
    if value.chars().count() > maximum || (!empty && value.is_empty()) {
        return Err(RulesServiceError::Invalid(label));
    }
    Ok(value.to_owned())
}

fn empty_text(value: Option<&Value>, maximum: usize) -> Result<String, RulesServiceError> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(value) => text(Some(value), "text", maximum, true),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LocalRuleWatchFiles, MODEL_STATUS_HISTORY, RuleWatchFiles, RulesServiceError, StatusError,
        model_directory, prepared_python_test_support, rules,
    };
    use rusqlite::params;
    use serde_json::{Value, json};
    use studio_runtime_store::{
        AgentRecord, AgentRepository, MonitorRecord, RulesRepository, RuntimeStore, StoreError,
        Table,
    };
    use tempfile::TempDir;

    fn fixture() -> (TempDir, RuntimeStore) {
        let directory = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(directory.path())).expect("runtime store");
        store
            .transaction_with(|tx| {
                for record in [
                    json!({
                        "id":"lead","rootId":"lead","epoch":3,"status":"running",
                        "isLead":true,"autoWake":true,"cwd":directory.path(),
                        "concurrency":2,"maxAgents":8,
                        "workerDefaults":{"model":"gpt-fixture","effort":"high","fastMode":false}
                    }),
                    json!({"id":"queued","rootId":"lead","epoch":4,"status":"queued","autoWake":false,"cwd":directory.path()}),
                    json!({"id":"done","rootId":"lead","epoch":2,"status":"completed","autoWake":false,"cwd":directory.path()}),
                ] {
                    AgentRepository::put(
                        tx,
                        &AgentRecord::from_value(record).expect("valid agent record"),
                    )?;
                }
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES('m-done',?1)",
                    [json!({"id":"m-done","agent":"done","status":"failed","exitCode":9,"error":"failed"}).to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("seed state");
        (directory, store)
    }

    #[test]
    fn status_snapshot_tracks_capacity_finished_pages_and_revision_replay() {
        let (_directory, store) = fixture();
        let first = model_directory(
            &store,
            "lead",
            3,
            &json!({"include_finished":true,"limit":1}),
            100.0,
        )
        .expect("status snapshot");
        assert_eq!(first["capacity"]["teamLimit"], 2);
        assert_eq!(first["capacity"]["teamActive"], 1);
        assert_eq!(first["capacity"]["queued"][0]["reasons"], json!(["paused"]));
        assert_eq!(first["finished"]["total"], 2);
        assert_eq!(
            first["finished"]["items"].as_array().expect("page").len(),
            1
        );
        let cursor = first["finished"]["nextCursor"].as_str().expect("next page");
        let second = model_directory(
            &store,
            "lead",
            3,
            &json!({"include_finished":true,"limit":1,"cursor":cursor}),
            100.0,
        )
        .expect("second status page");
        assert_eq!(second["finished"]["items"][0]["id"], "m-done");
        let replay = model_directory(
            &store,
            "lead",
            3,
            &json!({"since_revision":first["revision"]}),
            102.0,
        )
        .expect("status revision replay");
        assert_eq!(replay["unchanged"], true);
        assert_eq!(replay["changes"], json!([]));
        assert_eq!(replay["workerDefaults"]["model"], "gpt-fixture");
    }

    #[test]
    fn status_projection_matches_python_on_team_capacity_and_finished_page_corpus() {
        use std::process::Command;
        use std::time::Duration;

        let (_directory, store) = fixture();
        let args = json!({"include_finished":true,"limit":1});
        let actual =
            model_directory(&store, "lead", 3, &args, 100.0).expect("Rust status projection");
        let (agents, monitors) = {
            let connection = store.read().expect("read status corpus rows");
            (
                Table::<AgentRecord>::new(&connection)
                    .list()
                    .expect("read agent rows")
                    .into_iter()
                    .map(|record| record.value().clone())
                    .collect::<Vec<_>>(),
                Table::<MonitorRecord>::new(&connection)
                    .list()
                    .expect("read monitor rows")
                    .into_iter()
                    .map(|record| record.value().clone())
                    .collect::<Vec<_>>(),
            )
        };
        let payload = json!({
            "actor":"lead",
            "args":args,
            "agents":agents,
            "monitors":monitors,
        });
        let server_source =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/managed_status_watch_oracle.py");
        let mut command = Command::new(prepared_python_test_support::prepared_python(
            &server_source,
        ));
        command.arg(&script).arg(payload.to_string());
        let isolated = prepared_python_test_support::IsolatedTestEnvironment::new();
        isolated.apply(&mut command);
        let output = prepared_python_test_support::output_with_timeout(
            &mut command,
            &script,
            Duration::from_secs(60),
        )
        .expect("Python status oracle completes");
        assert!(
            output.status.success(),
            "Python status oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
        let mut comparable = actual;
        for result in [&mut comparable, &mut expected] {
            result
                .as_object_mut()
                .expect("status object")
                .remove("revision");
            if let Some(finished) = result.get_mut("finished") {
                finished
                    .as_object_mut()
                    .expect("finished object")
                    .remove("revision");
                finished
                    .as_object_mut()
                    .expect("finished object")
                    .remove("nextCursor");
            }
        }
        assert_eq!(comparable, expected, "status corpus differs from Python");
    }

    #[test]
    fn malformed_cached_status_corpus_is_rejected_with_table_and_actor() {
        use std::process::Command;
        use std::time::Duration;

        let (_directory, store) = fixture();
        let (agents, monitors) = {
            let connection = store.read().expect("read corpus rows");
            (
                Table::<AgentRecord>::new(&connection)
                    .list()
                    .expect("read agent rows")
                    .into_iter()
                    .map(|record| record.value().clone())
                    .collect::<Vec<_>>(),
                Table::<MonitorRecord>::new(&connection)
                    .list()
                    .expect("read monitor rows")
                    .into_iter()
                    .map(|record| record.value().clone())
                    .collect::<Vec<_>>(),
            )
        };
        let malformed = json!([{}, {"kind":"agent","id":"partial"}]);
        let payload = json!({
            "actor":"lead",
            "args":{},
            "agents":agents,
            "monitors":monitors,
            "corruptPrevious":malformed,
        });
        let server_source =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/managed_status_watch_oracle.py");
        let mut command = Command::new(prepared_python_test_support::prepared_python(
            &server_source,
        ));
        command.arg(&script).arg(payload.to_string());
        let isolated = prepared_python_test_support::IsolatedTestEnvironment::new();
        isolated.apply(&mut command);
        let output = prepared_python_test_support::output_with_timeout(
            &mut command,
            &script,
            Duration::from_secs(60),
        )
        .expect("malformed status corpus completes");
        assert!(
            output.status.success(),
            "malformed status corpus failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Value = serde_json::from_slice(&output.stdout).expect("corpus JSON");
        assert_eq!(expected["malformedPreviousRejected"], true);

        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_model_status(agent,revision,at,record) VALUES('lead','corrupt',1,?1)",
                    [malformed.to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("seed malformed cached status");
        let error = model_directory(
            &store,
            "lead",
            3,
            &json!({"since_revision":"corrupt"}),
            100.0,
        )
        .expect_err("reject malformed status cache");
        assert!(matches!(
            error,
            StatusError::Store(StoreError::InvalidStoredRecord { object: "runtime_model_status", id })
                if id == "lead"
        ));
    }

    #[test]
    fn file_rule_save_pause_and_post_commit_cancellations_match_python() {
        use std::process::Command;
        use std::time::Duration;

        let (directory, store) = fixture();
        let server_source =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/managed_rules_oracle.py");
        let mut command = Command::new(prepared_python_test_support::prepared_python(
            &server_source,
        ));
        command.arg(&script).arg(directory.path());
        let isolated = prepared_python_test_support::IsolatedTestEnvironment::new();
        isolated.apply(&mut command);
        let output = prepared_python_test_support::output_with_timeout(
            &mut command,
            &script,
            Duration::from_secs(60),
        )
        .expect("Python rules oracle completes");
        assert!(
            output.status.success(),
            "Python rules oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Value = serde_json::from_slice(&output.stdout).expect("rules oracle JSON");

        let save = json!({
            "id":"rule-fixed",
            "kind":"file",
            "name":"watch",
            "path":"watch.txt",
            "intervalSeconds":30,
        });
        let saved = rules(
            &store,
            "lead",
            3,
            Some(&save),
            "rule-save-request",
            100.0,
            &LocalRuleWatchFiles,
        )
        .expect("save file rule");
        let replayed = rules(
            &store,
            "lead",
            3,
            Some(&save),
            "rule-save-request",
            101.0,
            &LocalRuleWatchFiles,
        )
        .expect("replay exact rule request");
        assert_eq!(replayed, saved, "exact request returns saved result");
        let mismatched = json!({
            "id":"rule-fixed",
            "kind":"file",
            "name":"different",
            "path":"watch.txt",
            "intervalSeconds":30,
        });
        assert!(
            rules(
                &store,
                "lead",
                3,
                Some(&mismatched),
                "rule-save-request",
                102.0,
                &LocalRuleWatchFiles,
            )
            .is_err()
        );
        let persisted_after_mismatch = RulesRepository::get(
            &store.read().expect("read saved rule after conflict"),
            "rule-fixed",
        )
        .expect("read rule row")
        .expect("saved rule remains");
        assert_eq!(persisted_after_mismatch.value(), &saved.value);
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES('watch-monitor',?1)",
                    [json!({
                        "id":"watch-monitor",
                        "agent":"lead",
                        "ruleId":"rule-fixed",
                        "status":"running",
                    })
                    .to_string()],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("seed cancellation monitor");
        let paused = rules(
            &store,
            "lead",
            3,
            Some(&json!({"action":"pause","id":"rule-fixed"})),
            "rule-pause-request",
            100.0,
            &LocalRuleWatchFiles,
        )
        .expect("pause file rule");

        assert_eq!(saved.value, expected["saved"]);
        assert_eq!(replayed.value, expected["replayed"]);
        assert_eq!(expected["mismatch"], true);
        assert_eq!(paused.value, expected["paused"]);
        assert_eq!(expected["cancelled"], json!(["watch-monitor"]));
        assert_eq!(paused.cancel_monitor_ids, ["watch-monitor"]);
        let sync_payload: Value = store
            .read()
            .expect("read rule sync projection")
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='rule' AND id='rule-fixed'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|payload| serde_json::from_str(&payload).expect("valid sync payload"))
            .expect("rule sync projection exists");
        assert_eq!(sync_payload, expected["syncEntity"]);
    }

    #[test]
    fn status_validation_and_stale_epoch_leave_revision_table_unchanged() {
        let (_directory, store) = fixture();
        assert!(model_directory(&store, "lead", 2, &json!({}), 100.0).is_err());
        assert!(
            model_directory(&store, "lead", 3, &json!({"include_finished":"yes"}), 100.0).is_err()
        );
        let count: i64 = store
            .read()
            .expect("read state")
            .query_row("SELECT COUNT(*) FROM runtime_model_status", [], |row| {
                row.get(0)
            })
            .expect("count status rows");
        assert_eq!(count, 0);
    }

    #[test]
    fn status_cache_pruning_failure_rolls_back_new_revision() {
        let (_directory, store) = fixture();
        store
            .transaction_with(|tx| {
                for index in 0..MODEL_STATUS_HISTORY {
                    tx.execute(
                        "INSERT INTO runtime_model_status(agent,revision,at,record) VALUES('lead',?1,?2,'[]')",
                        params![format!("prior-{index}"), index as f64],
                    )?;
                }
                tx.execute_batch(
                    "CREATE TRIGGER fail_status_prune BEFORE DELETE ON runtime_model_status \
                     BEGIN SELECT RAISE(ABORT,'status prune failpoint'); END;",
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("seed revision history and pruning failpoint");
        let before = {
            let connection = store.read().expect("read old revisions");
            let mut statement = connection
                .prepare("SELECT revision,at,record FROM runtime_model_status WHERE agent='lead' ORDER BY revision")
                .expect("prepare old revision query");
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .expect("query old revisions")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect old revisions")
        };
        assert!(model_directory(&store, "lead", 3, &json!({}), 100.0).is_err());
        let after = {
            let connection = store.read().expect("read revisions after rollback");
            let mut statement = connection
                .prepare("SELECT revision,at,record FROM runtime_model_status WHERE agent='lead' ORDER BY revision")
                .expect("prepare revision query");
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .expect("query revisions")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect revisions")
        };
        assert_eq!(after, before, "failed pruning rolls back the insert");
    }

    #[derive(Clone, Copy)]
    struct Files;
    impl RuleWatchFiles for Files {
        fn resolve(&self, cwd: &str, path: &str) -> Result<String, RulesServiceError> {
            Ok(format!("{cwd}/{path}"))
        }
        fn fingerprint(&self, _: &str) -> Result<Option<Value>, RulesServiceError> {
            Ok(Some(json!([123, 4, 5])))
        }
    }

    #[test]
    fn rules_save_replays_by_request_identity_and_pause_returns_committed_cancellations() {
        let (_directory, store) = fixture();
        let save = json!({"kind":"file","name":"watch","path":"notes.txt","intervalSeconds":30});
        let first =
            rules(&store, "lead", 3, Some(&save), "request-1", 100.0, &Files).expect("save rule");
        let repeated = rules(&store, "lead", 3, Some(&save), "request-1", 101.0, &Files)
            .expect("retry same rule identity");
        assert_eq!(first.value["id"], repeated.value["id"]);
        let id = first.value["id"].as_str().expect("rule id").to_owned();
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES('watch-monitor',?1)",
                    [
                        json!({"id":"watch-monitor","agent":"lead","ruleId":id,"status":"running"})
                            .to_string(),
                    ],
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("seed monitor");
        let pause = rules(
            &store,
            "lead",
            3,
            Some(&json!({"action":"pause","id":id})),
            "request-pause",
            102.0,
            &Files,
        )
        .expect("pause rule");
        assert_eq!(pause.value["status"], "paused");
        assert_eq!(pause.cancel_monitor_ids, ["watch-monitor"]);
        let stored = RulesRepository::get(&store.read().expect("read state"), &id)
            .expect("read rule")
            .expect("rule exists");
        assert_eq!(stored.get("status"), Some(&json!("paused")));
    }

    #[derive(Clone, Copy)]
    struct FailingFiles;
    impl RuleWatchFiles for FailingFiles {
        fn resolve(&self, _: &str, _: &str) -> Result<String, RulesServiceError> {
            Err(RulesServiceError::FilePath)
        }
        fn fingerprint(&self, _: &str) -> Result<Option<Value>, RulesServiceError> {
            Err(RulesServiceError::FilePath)
        }
    }

    #[test]
    fn rule_file_preparation_failure_and_sql_failpoint_do_not_partially_write() {
        let (_directory, store) = fixture();
        let file_rule = json!({"kind":"file","name":"watch","path":"notes.txt"});
        assert!(
            rules(
                &store,
                "lead",
                3,
                Some(&file_rule),
                "failed-preparation",
                100.0,
                &FailingFiles,
            )
            .is_err()
        );
        let rule: i64 = store
            .read()
            .expect("read state")
            .query_row("SELECT COUNT(*) FROM runtime_rules", [], |row| row.get(0))
            .expect("count rules");
        assert_eq!(rule, 0);
        store
            .transaction_with(|tx| {
                tx.execute_batch(
                    "CREATE TRIGGER fail_rule_documents BEFORE INSERT ON sync_entities \
                     WHEN NEW.collection='rule' BEGIN SELECT RAISE(ABORT,'rule sync failpoint'); END;",
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("install failpoint");
        let interval_rule = json!({"kind":"interval","name":"rule"});
        assert!(
            rules(
                &store,
                "lead",
                3,
                Some(&interval_rule),
                "failed-commit",
                100.0,
                &LocalRuleWatchFiles,
            )
            .is_err()
        );
        let rule: i64 = store
            .read()
            .expect("read state")
            .query_row("SELECT COUNT(*) FROM runtime_rules", [], |row| row.get(0))
            .expect("count rules after rollback");
        assert_eq!(rule, 0);
        store
            .transaction_with(|tx| {
                tx.execute_batch(
                    "DROP TRIGGER fail_rule_documents;
                     CREATE TRIGGER fail_rule_rows BEFORE INSERT ON runtime_rules \
                     WHEN NEW.id='rule-write-fail' BEGIN SELECT RAISE(ABORT,'rule row failpoint'); END;",
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("install post-projection failpoint");
        assert!(
            rules(
                &store,
                "lead",
                3,
                Some(&json!({"id":"rule-write-fail","kind":"interval","name":"rule"})),
                "failed-rule-row",
                100.0,
                &LocalRuleWatchFiles,
            )
            .is_err()
        );
        let (rule_count, sync_count): (i64, i64) = {
            let connection = store.read().expect("read failed commit state");
            (
                connection
                    .query_row("SELECT COUNT(*) FROM runtime_rules", [], |row| row.get(0))
                    .expect("count rule rows"),
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM sync_entities WHERE collection='rule'",
                        [],
                        |row| row.get(0),
                    )
                    .expect("count rule sync rows"),
            )
        };
        assert_eq!(rule_count, 0);
        assert_eq!(sync_count, 0);
    }
}
