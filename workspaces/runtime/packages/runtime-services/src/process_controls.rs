//! Process identities and durable action receipts shared with the Python backend.
//! Listing is read-only; native effects are driven only after an action is claimed.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::process_controls::{
    ProcessActionResult, ProcessActionStatus, ProcessControlRequest, ProcessEntry, ProcessOutcome,
    ProcessReceipt, ProcessResponseTarget,
};
use studio_runtime_store::{
    ProcessActionKind, ProcessActionPhase, ProcessActionRecord, StoreError, Transaction,
};

use crate::account_session_runtime::ConnectionScope;
use crate::event_write::{CallerAdmission, RequestKey, begin_request};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Request id expired")]
    Expired,
    #[error("{0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
}
/// Separate the current model connection from live source-process scopes.
/// These are cached observations, not permission to start or replay a command.
#[derive(Clone, Default)]
pub struct Connections {
    owners: BTreeMap<String, String>,
    sources: BTreeMap<String, ConnectionScope>,
    scopes: BTreeMap<String, ConnectionScope>,
    responses: BTreeMap<String, HeldResponseProof>,
}
impl FromIterator<(String, String)> for Connections {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self {
            owners: iter.into_iter().collect(),
            sources: BTreeMap::new(),
            scopes: BTreeMap::new(),
            responses: BTreeMap::new(),
        }
    }
}
impl<const N: usize> From<[(String, String); N]> for Connections {
    fn from(value: [(String, String); N]) -> Self {
        value.into_iter().collect()
    }
}
impl Connections {
    pub fn get(&self, owner: &str) -> Option<&String> {
        self.owners.get(owner)
    }
    pub fn observe_source(&mut self, scope: ConnectionScope) {
        self.sources.insert(scope.account.clone(), scope);
    }
    pub fn observe_owner(&mut self, owner: String, scope: ConnectionScope) {
        self.owners.insert(owner.clone(), scope.connection.clone());
        self.scopes.insert(owner, scope);
    }
    pub fn observe_response(&mut self, proof: HeldResponseProof) {
        self.responses
            .insert(text(&proof.owner, "id", "").into(), proof);
    }
}

/// Candidate metadata is not native proof. Only a fresh, exact-scope compact
/// thread read can authorize Stop, and its turn remains conditional at native dispatch.
pub fn held_response_candidate(owner: &Value) -> bool {
    owner["provider"] == "claude"
        && owner["executionMode"] != "vm"
        && owner["status"] == "interrupted"
        && owner["inFlight"] == false
        && owner["autoWake"] == false
        && owner["deleted"] != true
        && !studio_runtime_store::truthy(owner.get("deletedAt"))
        && owner["archived"] != true
        && !studio_runtime_store::truthy(owner.get("archivedAt"))
        && owner["accountTransferId"].is_null()
        && owner["executionMove"].is_null()
        && owner["epoch"].as_u64().is_some()
        && !text(owner, "id", "").is_empty()
        && !text(owner, "threadId", "").is_empty()
        && !text(owner, "turnId", "").is_empty()
        && owner["nativeResponseTurn"] == owner["turnId"]
        && owner["restartRecovery"]["stage"] == "held"
        && owner["disconnectRecovery"]["source"] == "restart"
        && ["restartRecovery", "disconnectRecovery"].iter().all(|key| {
            owner[key]["epoch"] == owner["epoch"]
                && owner[key]["accountKey"] == text(owner, "accountKey", "default")
                && owner[key]["threadId"] == owner["threadId"]
        })
}

/// Ephemeral observation, never a stored lifecycle transition or delivery permit.
/// Revalidation binds the unchanged actor and complete current native scope.
#[derive(Clone)]
pub struct HeldResponseProof {
    owner: Value,
    scope: ConnectionScope,
}
impl HeldResponseProof {
    pub fn observe(owner: Value, scope: ConnectionScope, reply: &Value) -> Option<Self> {
        (held_response_candidate(&owner)
            && scope.account == text(&owner, "accountKey", "default")
            && !scope.connection.is_empty()
            && reply["thread"]["id"] == owner["threadId"]
            && reply["thread"]["status"]["type"] == "active")
            .then_some(Self { owner, scope })
    }
    fn matches(&self, owner: &Value, scope: &ConnectionScope) -> bool {
        self.owner == *owner && self.scope == *scope && held_response_candidate(owner)
    }
}

fn retained_monitor_scope(
    tx: &Transaction<'_>,
    monitor: &studio_runtime_store::MonitorRecord,
    owner: &Value,
) -> Result<Option<ConnectionScope>, Error> {
    if monitor.status != studio_runtime_schema::studio_errors::MonitorStatus::Running
        || monitor.agent != text(owner, "id", "")
        || Some(monitor.epoch.get()) != owner["epoch"].as_i64()
        || owner["deleted"] == true
        || studio_runtime_store::truthy(owner.get("deletedAt"))
    {
        return Ok(None);
    }
    let Some(operation) = &monitor.operation else {
        return Ok(None);
    };
    if operation.agent.as_str() != text(owner, "id", "")
        || Some(operation.epoch.get()) != owner["epoch"].as_i64()
    {
        return Ok(None);
    }
    match crate::account_worker::retained::vm_actor(tx, operation.agent.as_str()) {
        Ok(false) => {}
        Err(crate::account_worker::WorkerError::Store(error)) => return Err(Error::Store(error)),
        _ => return Ok(None),
    }
    match crate::account_worker::retained::monitor_command_scope(tx, &monitor.id, operation) {
        Ok(scope) => Ok(scope),
        Err(crate::account_worker::WorkerError::Store(error)) => Err(Error::Store(error)),
        _ => Ok(None),
    }
}

/// The source accounts of proven running host monitors. The HTTP caller only
/// observes those accounts' existing sessions; it never starts another session.
pub fn monitor_sources(
    tx: &Transaction<'_>,
    rows: &[(&Value, ProcessSource)],
) -> Result<Vec<ConnectionScope>, Error> {
    let mut scopes = BTreeMap::new();
    for (owner, source) in rows {
        if let ProcessSource::Monitor(monitor) = source
            && let Some(scope) = retained_monitor_scope(tx, monitor, owner)?
        {
            scopes.insert(scope.account.clone(), scope);
        }
    }
    Ok(scopes.into_values().collect())
}

/// Projection of an existing command, authorized by its original command ledger
/// and an observed live source scope, never by the actor's new model thread.
pub fn project_monitor(
    tx: &Transaction<'_>,
    raw: &studio_runtime_store::MonitorRecord,
    owner: &Value,
    owner_connection: Option<&str>,
    source: Option<&ConnectionScope>,
) -> Result<Value, Error> {
    let proven = match (retained_monitor_scope(tx, raw, owner)?, source) {
        (Some(original), Some(live))
            if live.account == original.account
                && live.generation == original.generation
                && !live.connection.is_empty()
                && (live.connection == original.connection
                    || raw.reattached_connection_id.as_deref()
                        == Some(live.connection.as_str())) =>
        {
            Some(live)
        }
        _ => None,
    };
    Ok(project_monitor_bound(
        raw,
        owner,
        proven
            .map(|scope| scope.connection.as_str())
            .or(owner_connection),
        proven.map(|scope| scope.generation),
    ))
}
fn project_monitor_bound(
    monitor: &studio_runtime_store::MonitorRecord,
    owner: &Value,
    connection: Option<&str>,
    generation: Option<u64>,
) -> Value {
    use studio_runtime_schema::studio_errors::MonitorStatus;
    let agent = monitor.agent.as_str();
    let account = text(owner, "accountKey", "default");
    let pending = matches!(
        monitor.status,
        MonitorStatus::Approval | MonitorStatus::Starting
    );
    let operation = monitor.operation.as_ref();
    let source_account = operation
        .map(|op| op.account_key.as_str())
        .or(pending.then_some(account));
    let mut source_connection = operation.map(|op| op.connection_id.as_str());
    let mut source_thread = operation.and_then(|op| op.evidence.thread_id.as_deref());
    if generation.is_some() {
        source_connection = connection;
    } else if monitor.status == MonitorStatus::Running
        && connection.is_some()
        && monitor.reattached_connection_id.as_deref() == connection
    {
        source_connection = connection;
        source_thread = owner["threadId"].as_str();
    }
    if generation.is_none()
        && source_thread.is_none_or(str::is_empty)
        && operation.map(|op| op.connection_id.as_str()) == connection
    {
        source_thread = owner["threadId"].as_str();
    }
    let epoch = monitor.epoch.get();
    let mut native = json!([
        monitor.id,
        epoch,
        source_account,
        source_connection,
        source_thread
    ]);
    if let Some(generation) = generation {
        native
            .as_array_mut()
            .expect("identity")
            .push(json!(generation));
    }
    let same = (generation.is_some() || source_account == Some(account))
        && owner["epoch"].as_i64() == Some(epoch)
        && connection.is_some_and(|c| !c.is_empty())
        && source_connection == connection
        && (generation.is_some()
            || (source_thread.is_some_and(|v| !v.is_empty())
                && source_thread == owner["threadId"].as_str()));
    let can_input = same
        && owner["autoWake"] == true
        && monitor.interactive
        && monitor.status == MonitorStatus::Running;
    let output = monitor.tail.as_str();
    let bytes = monitor.bytes.get() as u64;
    let command = monitor.command.as_str();
    let name = if command.is_empty() {
        "Command monitor"
    } else {
        command
    };
    let mut result = json!({"id":monitor.id,"key":URL_SAFE_NO_PAD.encode(json!(["monitor",agent,monitor.id]).to_string()),
        "generation":digest(&json!(["monitor",agent,native])),"agent":agent,"accountKey":account,"epoch":owner["epoch"],
        "source":"monitor","status":monitor.status,"cancelRequested":monitor.cancel_requested == Some(true),
        "created":monitor.created.seconds(),"name":name,"command":command,"cwd":monitor.cwd,"log":monitor.log,
        "output":output,"error":monitor.error.as_ref().map(|v| v.as_str()),"stdinError":monitor.stdin_error,
        "stdinClosed":monitor.stdin_closed == Some(true),"stdinCloseRequested":false,
        "bytes":bytes,"outputTruncated":monitor.output_truncated.unwrap_or(bytes > output.len() as u64),
        "capabilities":{"readOutput":true,"sendInput":can_input,"resize":can_input,"stop":same || (pending && owner["epoch"].as_i64() == Some(epoch) && source_account == Some(account))},
        "_raw":{"id":monitor.id,"status":monitor.status,"reattachedConnectionId":monitor.reattached_connection_id},
        "_sourceAccountKey":source_account,"_sourceConnectionId":source_connection,"_sourceThreadId":source_thread,"_sourceEpoch":epoch});
    if let Some(generation) = generation {
        result["_sourceGeneration"] = json!(generation);
    }
    if let Some(budget) = &monitor.output_wake_budget {
        result["outputWakeBudget"] = json!({"totalSuppressedMatches":budget.total_suppressed_matches,"paused":budget.paused,"tokens":budget.tokens});
    }
    if let Some(patterns) = &monitor.output_patterns {
        result["outputPatterns"] = json!(patterns);
    }
    result
}

fn raw(connection: &Connection, table: &str, id: &str) -> Result<Option<Value>, Error> {
    let text: Option<String> = connection
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?"),
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    text.map(|text| {
        serde_json::from_str(&text)
            .map_err(StoreError::from)
            .map_err(Error::from)
    })
    .transpose()
}
pub fn owner(connection: &Connection, scope: &str) -> Result<Value, Error> {
    let root =
        raw(connection, "runtime_agents", scope)?.ok_or(Error::Invalid("Unknown process owner"))?;
    if studio_runtime_store::truthy(root.get("deletedAt")) {
        return Err(Error::Invalid("This process owner was deleted"));
    }
    Ok(root)
}
pub fn owners(connection: &Connection, scope: &str) -> Result<Vec<Value>, Error> {
    let root = owner(connection, scope)?;
    if root["id"].as_str().is_none() {
        return Err(Error::Invalid("Invalid process owner"));
    }
    let mut statement = connection
        .prepare(
            "WITH RECURSIVE descendants(id) AS (
             SELECT ?1 UNION SELECT child.id FROM runtime_agents child
             JOIN descendants parent ON child.parent_id=parent.id
             WHERE child.deleted_at IS NULL
         ) SELECT agent.record FROM descendants JOIN runtime_agents agent ON agent.id=descendants.id
           WHERE descendants.id != ?1",
        )
        .map_err(StoreError::from)?;
    let rows = statement
        .query_map([scope], |row| row.get::<_, String>(0))
        .map_err(StoreError::from)?;
    let mut result = vec![root];
    for row in rows {
        result
            .push(serde_json::from_str(&row.map_err(StoreError::from)?).map_err(StoreError::from)?);
    }
    Ok(result)
}

/// A process source retains its owning record type through selection.
pub enum ProcessSource {
    Monitor(Box<studio_runtime_store::MonitorRecord>),
    Task(Value),
}

/// Scoped active commands paired with their already-loaded owners.
pub fn process_rows<'a>(
    connection: &Connection,
    owners: &'a [Value],
) -> Result<Vec<(&'a Value, ProcessSource)>, Error> {
    let mut result = Vec::new();
    for table in ["runtime_monitors", "runtime_tasks"] {
        let (index, status) = if table == "runtime_monitors" {
            ("runtime_monitor_agent_status", "IN (?2,?3,?4)")
        } else {
            ("runtime_task_agent_created_id", "= ?2")
        };
        let sql = format!(
            "SELECT record FROM {table} INDEXED BY {index} WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.status') {status}"
        );
        let mut statement = connection.prepare_cached(&sql).map_err(StoreError::from)?;
        for owner in owners {
            let id = text(owner, "id", "");
            let statuses = studio_runtime_store::startup_recovery::ACTIVE_MONITOR_STATUSES;
            let args = [id, statuses[0], statuses[1], statuses[2]];
            let args = if table == "runtime_monitors" {
                &args[..]
            } else {
                &args[..2]
            };
            let rows = statement
                .query_map(rusqlite::params_from_iter(args), |row| {
                    row.get::<_, String>(0)
                })
                .map_err(StoreError::from)?;
            for row in rows {
                let raw = row.map_err(StoreError::from)?;
                let source = if table == "runtime_monitors" {
                    ProcessSource::Monitor(Box::new(
                        serde_json::from_str(&raw).map_err(StoreError::from)?,
                    ))
                } else {
                    ProcessSource::Task(serde_json::from_str(&raw).map_err(StoreError::from)?)
                };
                result.push((owner, source));
            }
        }
    }
    Ok(result)
}

fn text<'a>(value: &'a Value, key: &str, default: &'a str) -> &'a str {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
}
fn string(value: &Value, key: &str) -> Value {
    value[key].as_str().map_or(Value::Null, |v| json!(v))
}
pub fn digest(value: &Value) -> String {
    studio_runtime_store::sha256_hex(value.to_string().as_bytes())
}
fn request_digest(request: &ProcessControlRequest) -> Result<String, Error> {
    let bytes = serde_json::to_vec(request).map_err(StoreError::from)?;
    Ok(studio_runtime_store::sha256_hex(&bytes))
}
pub fn project(source: &str, raw: &Value, owner: &Value, connection: Option<&str>) -> Value {
    let agent = text(raw, "agent", text(owner, "id", ""));
    let account = text(owner, "accountKey", "default");
    let source_account = string(raw, "sourceAccountKey");
    let source_connection = string(raw, "sourceConnectionId");
    let source_thread = string(raw, "sourceThreadId");
    let epoch = raw["ownerEpoch"].clone();
    let claude = source == "claude_task";
    let item = text(raw, "itemId", "");
    let opaque = if claude {
        item.strip_prefix("task:").unwrap_or(item)
    } else {
        text(raw, "processId", item)
    };
    let native = if claude {
        json!([
            opaque,
            epoch,
            source_account,
            source_connection,
            source_thread,
            raw["turnId"]
        ])
    } else {
        json!([
            opaque,
            item,
            epoch,
            source_account,
            source_connection,
            source_thread,
            raw["turnId"]
        ])
    };
    let same = source_account == account
        && epoch == owner["epoch"]
        && connection.is_some_and(|c| !c.is_empty())
        && source_connection.as_str() == connection
        && source_thread.as_str().is_some_and(|v| !v.is_empty())
        && source_thread == owner["threadId"];
    let can_input = same
        && owner["autoWake"] == true
        && !claude
        && raw["processId"].as_str().is_some_and(|v| !v.is_empty());
    let output = if claude { "" } else { text(raw, "tail", "") };
    let bytes = raw["bytes"].as_u64().unwrap_or(output.len() as u64);
    let default_name = if claude {
        "Claude background task"
    } else {
        "Background command"
    };
    let name = if claude {
        text(raw, "name", text(raw, "command", default_name))
    } else {
        text(raw, "command", text(raw, "name", default_name))
    };
    let mut result = json!({"id":text(raw,"id",opaque),"key":URL_SAFE_NO_PAD.encode(json!([source,agent,opaque]).to_string()),
        "generation":digest(&json!([source,agent,native])),"agent":agent,"accountKey":account,"epoch":owner["epoch"],
        "source":source,"status":text(raw,"status","unknown"),"cancelRequested":raw["cancelRequested"]==true,
        "created":raw["created"].as_f64().unwrap_or(0.0),"name":name,"command":string(raw,"command"),"cwd":string(raw,"cwd"),"log":string(raw,"log"),
        "output":output,"error":string(raw,"error"),"stdinError":string(raw,"stdinError"),"stdinClosed":raw["stdinClosed"]==true,
        "stdinCloseRequested":raw["stdinCloseRequested"]==true,"bytes":bytes,"outputTruncated":raw["outputTruncated"].as_bool().unwrap_or(bytes>output.len() as u64),
        "capabilities":{"readOutput":!claude,"sendInput":can_input,"resize":false,"stop":same},
        "_raw":raw,"_sourceAccountKey":source_account,"_sourceConnectionId":source_connection,"_sourceThreadId":source_thread,"_sourceEpoch":epoch});
    if claude && raw["outputFile"].is_string() {
        result["outputFile"] = raw["outputFile"].clone();
        result["capabilities"]["readOutput"] = json!(true);
    }
    result
}

#[cfg(test)]
fn legacy_project_bound(
    source: &str,
    raw: &Value,
    owner: &Value,
    connection: Option<&str>,
    retained_generation: Option<u64>,
) -> Value {
    let agent = text(raw, "agent", text(owner, "id", ""));
    let account = text(owner, "accountKey", "default");
    let mut source_account = string(raw, "sourceAccountKey");
    let mut source_connection = string(raw, "sourceConnectionId");
    let mut source_thread = string(raw, "sourceThreadId");
    let epoch;
    let opaque;
    let pending =
        source == "monitor" && matches!(raw["status"].as_str(), Some("starting" | "approval"));
    let is_monitor = source == "monitor";
    let is_claude = source == "claude_task";
    let native = if is_monitor {
        epoch = raw["epoch"].clone();
        let operation = &raw["operation"];
        source_account = operation
            .get("accountKey")
            .filter(|v| v.is_string() && **v != "")
            .cloned()
            .unwrap_or(if pending { json!(account) } else { Value::Null });
        source_connection = string(operation, "connectionId");
        source_thread = string(operation, "threadId");
        if retained_generation.is_some() {
            source_connection = json!(connection);
        } else if raw["status"] == "running"
            && connection.is_some()
            && raw["reattachedConnectionId"].as_str() == connection
        {
            source_connection = json!(connection);
            source_thread = owner["threadId"].clone();
        }
        if retained_generation.is_none()
            && source_thread.as_str().is_none_or(str::is_empty)
            && operation["connectionId"].as_str() == connection
        {
            source_thread = owner["threadId"].clone();
        }
        opaque = text(raw, "id", "").to_owned();
        let mut identity = json!([
            raw["id"],
            epoch,
            source_account,
            source_connection,
            source_thread
        ]);
        if let Some(generation) = retained_generation {
            identity
                .as_array_mut()
                .expect("monitor identity")
                .push(json!(generation));
        }
        identity
    } else {
        epoch = raw["ownerEpoch"].clone();
        let item = text(raw, "itemId", "");
        opaque = if is_claude {
            item.strip_prefix("task:").unwrap_or(item).to_owned()
        } else {
            text(raw, "processId", item).to_owned()
        };
        if is_claude {
            json!([
                opaque,
                epoch,
                source_account,
                source_connection,
                source_thread,
                raw["turnId"]
            ])
        } else {
            json!([
                opaque,
                item,
                epoch,
                source_account,
                source_connection,
                source_thread,
                raw["turnId"]
            ])
        }
    };
    let same = (retained_generation.is_some() || source_account == account)
        && epoch == owner["epoch"]
        && connection.is_some_and(|c| !c.is_empty())
        && source_connection.as_str() == connection
        && (retained_generation.is_some()
            || (source_thread.as_str().is_some_and(|v| !v.is_empty())
                && source_thread == owner["threadId"]));
    let can_input = same
        && owner["autoWake"] == true
        && if is_monitor {
            raw["interactive"] == true && raw["status"] == "running"
        } else {
            !is_claude && raw["processId"].as_str().is_some_and(|v| !v.is_empty())
        };
    let output = if is_claude { "" } else { text(raw, "tail", "") };
    let bytes = raw["bytes"].as_u64().unwrap_or(output.len() as u64);
    let default_name = if is_monitor {
        "Command monitor"
    } else if is_claude {
        "Claude background task"
    } else {
        "Background command"
    };
    let name = if is_claude {
        text(raw, "name", text(raw, "command", default_name))
    } else {
        text(raw, "command", text(raw, "name", default_name))
    };
    let mut result = json!({"id":text(raw,"id",&opaque),"key":URL_SAFE_NO_PAD.encode(json!([source,agent,opaque]).to_string()),
        "generation":digest(&json!([source,agent,native])),"agent":agent,"accountKey":account,"epoch":owner["epoch"],
        "source":source,"status":text(raw,"status","unknown"),"cancelRequested":raw["cancelRequested"]==true,
        "created":raw["created"].as_f64().unwrap_or(0.0),"name":name,"command":string(raw,"command"),"cwd":string(raw,"cwd"),"log":string(raw,"log"),
        "output":output,"error":string(raw,"error"),"stdinError":string(raw,"stdinError"),"stdinClosed":raw["stdinClosed"]==true,
        "stdinCloseRequested":raw["stdinCloseRequested"]==true,"bytes":bytes,"outputTruncated":raw["outputTruncated"].as_bool().unwrap_or(bytes>output.len() as u64),
        "capabilities":{"readOutput":!is_claude,"sendInput":can_input,"resize":is_monitor&&can_input,"stop":same||(pending&&epoch==owner["epoch"]&&source_account==account)},
        "_raw":raw,"_sourceAccountKey":source_account,"_sourceConnectionId":source_connection,"_sourceThreadId":source_thread,"_sourceEpoch":epoch});
    if let Some(generation) = retained_generation {
        result["_sourceGeneration"] = json!(generation);
    }
    if is_monitor {
        if let Some(budget) = raw.get("outputWakeBudget") {
            result["outputWakeBudget"] = json!({"totalSuppressedMatches":budget["totalSuppressedMatches"],"paused":budget["paused"],"tokens":budget["tokens"]});
        }
        if let Some(patterns) = raw.get("outputPatterns") {
            result["outputPatterns"] = patterns.clone();
        }
    }
    if is_claude && raw["outputFile"].is_string() {
        result["outputFile"] = raw["outputFile"].clone();
        result["capabilities"]["readOutput"] = json!(true);
    }
    result
}
pub fn processes(
    connection: &Transaction<'_>,
    rows: &[(&Value, ProcessSource)],
    connections: &Connections,
) -> Result<Vec<Value>, Error> {
    let mut output = Vec::new();
    for (owner, row) in rows {
        let owner_connection = connections.get(text(owner, "id", "")).map(String::as_str);
        match row {
            ProcessSource::Monitor(monitor) => {
                let account = monitor
                    .operation
                    .as_ref()
                    .map(|op| op.account_key.as_str())
                    .unwrap_or("default");
                output.push(project_monitor(
                    connection,
                    monitor,
                    owner,
                    owner_connection,
                    connections.sources.get(account),
                )?);
            }
            ProcessSource::Task(raw) => {
                let mut raw = raw.clone();
                let provider = text(
                    &raw,
                    "sourceProvider",
                    text(owner, "provider", ProviderKind::Codex.key()),
                )
                .to_owned();
                let source = if raw["kind"] == "command"
                    && studio_runtime_store::truthy(raw.get("processId"))
                {
                    "codex_task"
                } else if provider == ProviderKind::Claude.key()
                    && raw["kind"] == "tool"
                    && text(&raw, "itemId", "")
                        .strip_prefix("task:")
                        .is_some_and(|v| !v.is_empty())
                {
                    "claude_task"
                } else {
                    continue;
                };
                raw["sourceProvider"] = json!(provider);
                output.push(project(source, &raw, owner, owner_connection));
            }
        }
    }
    output.sort_by(|a, b| {
        b["created"]
            .as_f64()
            .partial_cmp(&a["created"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| text(a, "key", "").cmp(text(b, "key", "")))
    });
    Ok(output)
}
pub fn response_target(owner: &Value, connection: Option<&str>) -> Value {
    if owner["inFlight"] == true
        && connection.is_some()
        && !text(owner, "threadId", "").is_empty()
        && !text(owner, "turnId", "").is_empty()
    {
        response_identity(owner, connection.unwrap_or_default())
    } else {
        Value::Null
    }
}
fn stop_target(owner: &Value, connections: &Connections) -> Value {
    let id = text(owner, "id", "");
    let active = response_target(owner, connections.get(id).map(String::as_str));
    if !active.is_null() {
        return active;
    }
    match (connections.responses.get(id), connections.scopes.get(id)) {
        (Some(proof), Some(scope)) if proof.matches(owner, scope) => {
            response_identity(owner, &scope.connection)
        }
        _ => Value::Null,
    }
}
fn response_identity(owner: &Value, connection: &str) -> Value {
    json!({"agent":owner["id"],"accountKey":text(owner,"accountKey","default"),"epoch":owner["epoch"],"connectionId":connection,"threadId":owner["threadId"],"turnId":owner["turnId"]})
}
pub fn listing(
    connection: &Transaction<'_>,
    owners: &[Value],
    rows: &[(&Value, ProcessSource)],
    connections: &Connections,
) -> Result<Value, Error> {
    let mut processes = processes(connection, rows, connections)?;
    for p in &mut processes {
        p.as_object_mut()
            .expect("process object")
            .retain(|k, _| !k.starts_with('_'));
    }
    Ok(json!({"responseTarget":stop_target(&owners[0],connections),"processes":processes}))
}
pub fn matches(process: &Value, identity: &Value) -> bool {
    ["key", "generation", "agent", "accountKey", "epoch"]
        .iter()
        .all(|k| process[k] == identity[k])
}
fn response_matches(expected: &Value, target: &ProcessResponseTarget) -> bool {
    expected.as_object().is_some_and(|object| object.len() == 6)
        && expected["agent"] == target.agent.as_str()
        && expected["accountKey"] == target.account_key.as_str()
        && expected["epoch"].as_u64() == Some(target.epoch)
        && expected["connectionId"] == target.connection_id.as_str()
        && expected["threadId"] == target.thread_id.as_str()
        && expected["turnId"] == target.turn_id.as_str()
}
pub fn validate_response(
    owner: &Value,
    target: &ProcessResponseTarget,
    connection: Option<&str>,
) -> bool {
    response_matches(&response_target(owner, connection), target)
}
pub fn validate_stop_response(
    owner: &Value,
    target: &ProcessResponseTarget,
    scope: &ConnectionScope,
    proof: Option<&HeldResponseProof>,
) -> bool {
    validate_response(owner, target, Some(&scope.connection))
        || proof.is_some_and(|proof| {
            proof.matches(owner, scope)
                && response_matches(&response_identity(owner, &scope.connection), target)
        })
}

pub enum Claim {
    Replay(ProcessActionResult),
    New {
        record: Box<ProcessActionRecord>,
        targets: Vec<Value>,
        result: ProcessActionResult,
    },
}

pub fn claim(
    tx: &Transaction<'_>,
    request: &ProcessControlRequest,
    connections: &Connections,
    now: f64,
) -> Result<Claim, Error> {
    if let Some(result) = replay(tx, request)? {
        return Ok(Claim::Replay(result));
    }
    let command = request;
    let command_scope = command.scope();
    let id = command_scope.request_id.as_str();
    let signature = request_digest(request)?;
    let scope = command_scope.agent.as_str();
    let owners = owners(tx, scope)?;
    let rows = process_rows(tx, &owners)?;
    let processes = processes(tx, &rows, connections)?;
    let owner = &owners[0];
    if text(owner, "accountKey", "default") != command_scope.account_key.as_str()
        || owner["epoch"].as_u64() != Some(command_scope.epoch)
    {
        return Err(Error::Invalid(
            "The process owner changed. Refresh the process list",
        ));
    }
    let mut targets = Vec::new();
    let mut entries = Vec::new();
    let response = if let ProcessControlRequest::StopResponse {
        response_target, ..
    }
    | ProcessControlRequest::BackgroundCommand {
        response_target, ..
    } = command
    {
        let expected = if matches!(command, ProcessControlRequest::StopResponse { .. }) {
            stop_target(owner, connections)
        } else {
            self::response_target(owner, connections.get(scope).map(String::as_str))
        };
        let valid = response_matches(&expected, response_target);
        if !valid {
            return Err(Error::Invalid(
                "The response changed. Refresh the process list",
            ));
        }
        Some(serde_json::value::to_raw_value(response_target).map_err(StoreError::from)?)
    } else {
        let (selected, include_descendants) = match command {
            ProcessControlRequest::StopSelected {
                process,
                include_descendants,
                ..
            } => (std::slice::from_ref(process), *include_descendants),
            ProcessControlRequest::StopOwned {
                processes,
                include_descendants,
                ..
            } => (processes.as_slice(), *include_descendants),
            ProcessControlRequest::StopResponse { .. }
            | ProcessControlRequest::BackgroundCommand { .. } => {
                unreachable!("response commands handled above")
            }
        };
        let mut seen = HashSet::new();
        for identity in selected {
            let found = processes.iter().find(|p| {
                text(p, "key", "") == identity.key.as_str()
                    && text(p, "generation", "") == identity.generation.as_str()
                    && text(p, "agent", "") == identity.agent.as_str()
                    && text(p, "accountKey", "") == identity.account_key.as_str()
                    && p["epoch"].as_u64() == Some(identity.epoch)
            });
            let error = if !seen.insert(identity.key.as_str()) {
                Some("A process identity was repeated")
            } else if !include_descendants && identity.agent.as_str() != scope {
                Some("Worker processes are outside this stop scope")
            } else if found.is_none() {
                Some("This process changed or finished. Refresh the process list")
            } else if found.is_some_and(|p| p["capabilities"]["stop"] != true) {
                Some("The process owner changed. Refresh the process list")
            } else {
                None
            };
            if let Some(error) = error {
                entries.push(ProcessEntry {
                    key: identity.key.as_str().to_owned(),
                    agent: identity.agent.as_str().to_owned(),
                    source: None,
                    generation: None,
                    outcome: ProcessOutcome::Stale {
                        error: error.into(),
                    },
                    extra: Default::default(),
                });
            } else if let Some(process) = found {
                entries.push(ProcessEntry {
                    key: text(process, "key", "").to_owned(),
                    agent: text(process, "agent", "").to_owned(),
                    source: Some(text(process, "source", "").to_owned()),
                    generation: Some(text(process, "generation", "").to_owned()),
                    outcome: ProcessOutcome::Pending,
                    extra: Default::default(),
                });
                targets.push(process.clone());
            }
        }
        None
    };
    let result = ProcessActionResult {
        request_id: id.into(),
        status: ProcessActionStatus::Uncertain,
        processes: entries,
        response: None,
        extra: Default::default(),
    };
    let root_request_id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    let admitted = match begin_request::<ProcessActionResult>(
        tx,
        RequestKey::new(
            "process-action",
            &[
                scope,
                command_scope.account_key.as_str(),
                &command_scope.epoch.to_string(),
            ],
            &root_request_id,
        )
        .operation(&[id]),
        &signature,
        chrono::Utc::now().timestamp_millis(),
    )
    .map_err(StoreError::from)?
    {
        CallerAdmission::Fresh(admitted) => {
            admitted.derive_result("process-action", &[id], &signature)
        }
        CallerAdmission::Replay(_) => {
            return Err(Error::Store(StoreError::RequestResult(
                studio_runtime_store::request_results::RequestResultError::Conflict {
                    id: root_request_id,
                },
            )));
        }
    };
    let record = ProcessActionRecord {
        request_id: id.into(),
        root_request_id,
        admitted_request: admitted,
        signature: signature.clone(),
        phase: ProcessActionPhase::Dispatching,
        scope_agent: scope.into(),
        scope_account_key: command_scope.account_key.as_str().to_owned(),
        scope_epoch: command_scope.epoch.into(),
        action: match command {
            ProcessControlRequest::StopSelected { .. } => ProcessActionKind::StopSelected,
            ProcessControlRequest::StopOwned { .. } => ProcessActionKind::StopOwned,
            ProcessControlRequest::StopResponse { .. } => ProcessActionKind::StopResponse,
            ProcessControlRequest::BackgroundCommand { .. } => ProcessActionKind::BackgroundCommand,
        },
        response_target: response,
        processes: result.processes.clone(),
        created: now,
        result: result.clone(),
    };
    tx.execute(
        "INSERT INTO runtime_process_actions(id,signature,record) VALUES(?,?,?)",
        params![
            id,
            signature,
            serde_json::to_string(&record).map_err(StoreError::from)?
        ],
    )
    .map_err(StoreError::from)?;
    Ok(Claim::New {
        record: Box::new(record),
        targets,
        result,
    })
}
/// Reuse a durable action before any fresh native observation. Changed content
/// under the same request ID remains an error, including after a lost response.
pub fn replay(
    tx: &Transaction<'_>,
    request: &ProcessControlRequest,
) -> Result<Option<ProcessActionResult>, Error> {
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT signature,record FROM runtime_process_actions WHERE id=?",
            [request.scope().request_id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(StoreError::from)?;
    let Some((signature, raw)) = prior else {
        return Ok(None);
    };
    if signature != request_digest(request)? {
        return Err(Error::Invalid(
            "This process action ID has different content",
        ));
    }
    let record: ProcessActionRecord = serde_json::from_str(&raw).map_err(StoreError::from)?;
    Ok(Some(record.result))
}
pub fn save(tx: &Transaction<'_>, record: &ProcessActionRecord) -> Result<(), Error> {
    tx.execute(
        "UPDATE runtime_process_actions SET record=? WHERE id=?",
        params![
            serde_json::to_string(record).map_err(StoreError::from)?,
            record.request_id
        ],
    )
    .map_err(StoreError::from)?;
    Ok(())
}
pub fn receipt(connection: &Connection, query: &Value) -> Result<ProcessReceipt, Error> {
    let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_process_actions')",[],|r|r.get(0)).map_err(StoreError::from)?;
    if !exists {
        return input_receipt(connection, query);
    }
    let record: Option<String> = connection
        .query_row(
            "SELECT record FROM runtime_process_actions WHERE id=?",
            [text(query, "requestId", "")],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    let record: Option<ProcessActionRecord> = record
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(StoreError::from)?;
    if let Some(record) = record
        && record.scope_agent == query["agent"]
        && record.scope_account_key == query["accountKey"]
        && query["epoch"].as_f64() == record.scope_epoch.as_f64()
    {
        return Ok(ProcessReceipt::found(record.result));
    }
    input_receipt(connection, query)
}
fn input_receipt(connection: &Connection, query: &Value) -> Result<ProcessReceipt, Error> {
    crate::monitor_http_input::read(connection, query).map_err(|error| match error {
        crate::monitor_policy::MonitorError::Expired => Error::Expired,
        crate::monitor_policy::MonitorError::Store(error) => Error::Store(error),
        crate::monitor_policy::MonitorError::InvalidStored { .. }
        | crate::monitor_policy::MonitorError::InvalidResultReceipt(_)
        | crate::monitor_policy::MonitorError::InvalidDeliveryIdentity(_)
        | crate::monitor_policy::MonitorError::InvalidOutputReceipt(_)
        | crate::monitor_policy::MonitorError::TaskFailed
        | crate::monitor_policy::MonitorError::Refused(_)
        | crate::monitor_policy::MonitorError::InputOutcomeUnknown
        | crate::monitor_policy::MonitorError::Unavailable(_)
        | crate::monitor_policy::MonitorError::ProviderRequest { .. }
        | crate::monitor_policy::MonitorError::ConfigurationRejected { .. }
        | crate::monitor_policy::MonitorError::ConfigurationReceipt { .. }
        | crate::monitor_policy::MonitorError::Event(_)
        | crate::monitor_policy::MonitorError::Rules(_)
        | crate::monitor_policy::MonitorError::Io(_) => {
            Error::Invalid("Invalid monitor input receipt")
        }
    })
}

#[cfg(test)]
mod claim_identity_tests {
    use super::*;
    use studio_runtime_schema::persistence::RequestIdentity;
    use studio_runtime_store::request_results::RequestResults;

    #[test]
    fn typed_monitor_process_projection_preserves_identity_and_capabilities() {
        let owner = json!({"id":"owner","epoch":1,"accountKey":"default","threadId":"thread","autoWake":true});
        for status in ["starting", "running"] {
            let raw = json!({"id":"watch","agent":"owner","epoch":1,"status":status,"requestId":"request","created":1,"activityAt":1,"activityGeneration":0,"bytes":12,"command":"printf x","cwd":std::env::temp_dir(),"interactive":true,"livenessCommand":"","log":std::env::temp_dir().join("watch.log"),"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"x","timeout_ms":3600000,"wakeOn":"exit","operation":{"agent":"owner","epoch":1,"accountKey":"default","connectionId":"connection","threadId":"thread"}});
            let fixture = format!(
                r#"{{"id":"watch","agent":"owner","epoch":1,"status":"{status}","requestId":"request","created":1,"activityAt":1,"activityGeneration":0,"bytes":12,"command":"printf x","cwd":{},"interactive":true,"livenessCommand":"","log":{},"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"x","timeout_ms":3600000,"wakeOn":"exit","operation":{{"agent":"owner","epoch":1,"accountKey":"default","connectionId":"connection","threadId":"thread"}}}}"#,
                serde_json::to_string(&std::env::temp_dir()).unwrap(),
                serde_json::to_string(&std::env::temp_dir().join("watch.log")).unwrap()
            );
            let record: studio_runtime_store::MonitorRecord =
                serde_json::from_str(&fixture).unwrap();
            let mut expected =
                legacy_project_bound("monitor", &raw, &owner, Some("connection"), None);
            let mut actual = project_monitor_bound(&record, &owner, Some("connection"), None);
            expected.as_object_mut().unwrap().remove("_raw");
            actual.as_object_mut().unwrap().remove("_raw");
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn process_generation_digest_keeps_canonical_nested_keys() {
        let value = json!({"z":[{"b":2,"a":1}],"a":{"y":3,"b":2}});
        assert_eq!(
            digest(&value),
            "72ddf54a7a1042b66ae85c35ecc5c9b2d5fea1cce275577cf0502e1f1965da22"
        );
    }

    #[test]
    fn normalized_signature_replays_and_refuses_preupgrade_receipt() {
        let raw = r#"{"action":"stop_owned","requestId":"request","agent":"owner","accountKey":"acct","epoch":1.0,"processes":[{"key":"key","generation":"gen","agent":"owner","accountKey":"acct","epoch":1.0}],"responseTarget":null}"#;
        let request: ProcessControlRequest = serde_json::from_str(raw).expect("request");
        let normalized = raw
            .replace("1.0", "1")
            .replace(",\"responseTarget\":null", "");
        let retry: ProcessControlRequest = serde_json::from_str(&normalized).expect("retry");
        let signature = request_digest(&request).expect("signature");
        assert_eq!(signature, request_digest(&retry).expect("retry signature"));
        let old = "b16654521805af453f0fc6e0b9f42fb54887c3b20eb1fb103b50e05a645497f8";
        assert_ne!(signature, old);
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch(
            "CREATE TABLE runtime_process_actions(id TEXT PRIMARY KEY,signature TEXT,record TEXT);",
        )
        .expect("table");
        db.execute(
            "INSERT INTO runtime_process_actions VALUES('request',?1,'{}')",
            [old],
        )
        .expect("old receipt");
        let tx = db.transaction().expect("transaction");
        assert!(matches!(
            replay(&tx, &retry),
            Err(Error::Invalid(
                "This process action ID has different content"
            ))
        ));
    }

    #[test]
    fn process_claim_reuses_its_admitted_token_after_expiry_without_another_effect() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT,parent_id TEXT,deleted_at REAL);
            CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT, stream_pending INTEGER NOT NULL DEFAULT 0, monitor_status TEXT, monitor_error_candidate INTEGER NOT NULL DEFAULT 1, monitor_created REAL NOT NULL DEFAULT 0);
            CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(id);
            CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT);
            CREATE INDEX runtime_task_agent_created_id ON runtime_tasks(id);
            CREATE TABLE runtime_process_actions(id TEXT PRIMARY KEY,signature TEXT,record TEXT);
            CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT);
            CREATE TABLE effects(count INTEGER);
            INSERT INTO effects VALUES(0);")
            .expect("claim tables");
        studio_runtime_store::receipt_schema::create(&db).expect("receipt schema");
        let owner = json!({"id":"owner","epoch":3,"accountKey":"account","inFlight":true,
            "threadId":"thread","turnId":"turn"});
        db.execute(
            "INSERT INTO runtime_agents(id,record) VALUES('owner',?1)",
            [owner.to_string()],
        )
        .expect("owner");
        let raw = serde_json::value::RawValue::from_string(
            json!({"action":"stop_response","requestId":"opaque-action-id",
            "agent":"owner","accountKey":"account","epoch":3,"responseTarget":{
                "agent":"owner","accountKey":"account","epoch":3,"connectionId":"connection",
                "threadId":"thread","turnId":"turn"}})
            .to_string(),
        )
        .expect("fixture JSON");
        let request: ProcessControlRequest = serde_json::from_str(raw.get()).expect("request");
        let connections = Connections::from([("owner".into(), "connection".into())]);
        let tx = db.transaction().expect("transaction");
        let Claim::New { record, .. } =
            claim(&tx, &request, &connections, 1.0).expect("fresh claim")
        else {
            panic!("first claim must be new");
        };
        let root = &record.root_request_id;
        RequestIdentity::new("process-action", &[], root, &[]).expect("UUIDv7 root");
        assert_eq!(record.request_id, "opaque-action-id");
        tx.execute("UPDATE effects SET count=count+1", [])
            .expect("first dispatch");
        let token = record.admitted_request.clone();
        let after_ttl =
            token.created() + studio_runtime_schema::persistence::REQUEST_RESULT_TTL_MS + 1;
        RequestResults::gc(&tx, after_ttl).expect("expire fresh ingress");
        assert!(
            RequestIdentity::new("process-action", &[], root, &[])
                .expect("identity")
                .validate(
                    after_ttl,
                    RequestResults::expired_before_ms(&tx).expect("floor")
                )
                .is_err()
        );
        assert!(matches!(
            claim(&tx, &request, &connections, after_ttl as f64 / 1000.0)
                .expect("admitted replay never reenters ingress"),
            Claim::Replay(_)
        ));
        let persisted: String = tx
            .query_row(
                "SELECT record FROM runtime_process_actions WHERE id='opaque-action-id'",
                [],
                |row| row.get(0),
            )
            .expect("owner record");
        let persisted: ProcessActionRecord = serde_json::from_str(&persisted).expect("record");
        assert_eq!(&persisted.root_request_id, root);
        assert_eq!(persisted.admitted_request, token);
        assert_eq!(
            tx.query_row("SELECT count FROM effects", [], |row| row.get::<_, i64>(0))
                .expect("effects"),
            1
        );
    }
}
