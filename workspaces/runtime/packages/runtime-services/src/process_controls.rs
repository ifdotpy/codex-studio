//! Process identities and durable action receipts shared with the Python backend.
//! Listing is read-only; native effects are driven only after an action is claimed.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::process_controls::{
    ProcessActionResult, ProcessActionStatus, ProcessControlRequest, ProcessEntry, ProcessOutcome,
    ProcessReceipt, Request,
};
use studio_runtime_store::{StoreError, Transaction};

use crate::account_session_runtime::ConnectionScope;

#[derive(Debug, thiserror::Error)]
pub enum Error {
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
    monitor: &Value,
    owner: &Value,
) -> Result<Option<ConnectionScope>, Error> {
    if monitor["status"] != "running"
        || monitor["agent"] != owner["id"]
        || monitor["epoch"] != owner["epoch"]
        || owner["deleted"] == true
        || studio_runtime_store::truthy(owner.get("deletedAt"))
    {
        return Ok(None);
    }
    let Ok(operation) =
        serde_json::from_value::<crate::monitor_policy::Operation>(monitor["operation"].clone())
    else {
        return Ok(None);
    };
    if operation.agent != text(owner, "id", "") || Some(operation.epoch) != owner["epoch"].as_i64()
    {
        return Ok(None);
    }
    match crate::account_worker::retained::vm_actor(tx, &operation.agent) {
        Ok(false) => {}
        Err(crate::account_worker::WorkerError::Store(error)) => return Err(Error::Store(error)),
        _ => return Ok(None),
    }
    match crate::account_worker::retained::monitor_command_scope(
        tx,
        text(monitor, "id", ""),
        &operation,
    ) {
        Ok(scope) => Ok(scope),
        Err(crate::account_worker::WorkerError::Store(error)) => Err(Error::Store(error)),
        _ => Ok(None),
    }
}

/// The source accounts of proven running host monitors. The HTTP caller only
/// observes those accounts' existing sessions; it never starts another session.
pub fn monitor_sources(
    tx: &Transaction<'_>,
    rows: &[(bool, &Value, Value)],
) -> Result<Vec<ConnectionScope>, Error> {
    let mut scopes = BTreeMap::new();
    for (_, owner, monitor) in rows.iter().filter(|(monitor, _, _)| *monitor) {
        if let Some(scope) = retained_monitor_scope(tx, monitor, owner)? {
            scopes.insert(scope.account.clone(), scope);
        }
    }
    Ok(scopes.into_values().collect())
}

/// Projection of an existing command, authorized by its original command ledger
/// and an observed live source scope, never by the actor's new model thread.
pub fn project_monitor(
    tx: &Transaction<'_>,
    raw: &Value,
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
                    || raw["reattachedConnectionId"].as_str()
                        == Some(live.connection.as_str())) =>
        {
            Some(live)
        }
        _ => None,
    };
    Ok(project_bound(
        "monitor",
        raw,
        owner,
        proven
            .map(|scope| scope.connection.as_str())
            .or(owner_connection),
        proven.map(|scope| scope.generation),
    ))
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

/// Scoped active commands paired with their already-loaded owners.
pub fn process_rows<'a>(
    connection: &Connection,
    owners: &'a [Value],
) -> Result<Vec<(bool, &'a Value, Value)>, Error> {
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
                result.push((
                    table == "runtime_monitors",
                    owner,
                    serde_json::from_str(&row.map_err(StoreError::from)?)
                        .map_err(StoreError::from)?,
                ));
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
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let ordered: BTreeMap<_, _> = map.iter().collect();
            format!(
                "{{{}}}",
                ordered
                    .iter()
                    .map(|(key, value)| format!("{}:{}", json!(key), canonical(value)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.to_string(),
    }
}
pub fn digest(value: &Value) -> String {
    studio_runtime_store::sha256_hex(canonical(value).as_bytes())
}
pub fn project(source: &str, raw: &Value, owner: &Value, connection: Option<&str>) -> Value {
    project_bound(source, raw, owner, connection, None)
}
fn project_bound(
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
    let mut result = json!({"id":text(raw,"id",&opaque),"key":URL_SAFE_NO_PAD.encode(canonical(&json!([source,agent,opaque]))),
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
    rows: &[(bool, &Value, Value)],
    connections: &Connections,
) -> Result<Vec<Value>, Error> {
    let mut output = Vec::new();
    for (monitor, owner, raw) in rows {
        let mut raw = raw.clone();
        let provider = text(
            &raw,
            "sourceProvider",
            text(owner, "provider", ProviderKind::Codex.key()),
        )
        .to_owned();
        let source = if *monitor {
            "monitor"
        } else if raw["kind"] == "command" && studio_runtime_store::truthy(raw.get("processId")) {
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
        if source != "monitor" {
            raw["sourceProvider"] = json!(provider);
        }
        let owner_connection = connections.get(text(owner, "id", "")).map(String::as_str);
        output.push(if source == "monitor" {
            project_monitor(
                connection,
                &raw,
                owner,
                owner_connection,
                connections
                    .sources
                    .get(text(&raw["operation"], "accountKey", "default")),
            )?
        } else {
            project(source, &raw, owner, owner_connection)
        });
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
    rows: &[(bool, &Value, Value)],
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
pub fn validate_response(owner: &Value, target: &Value, connection: Option<&str>) -> bool {
    !target.is_null() && response_target(owner, connection) == *target
}
pub fn validate_stop_response(
    owner: &Value,
    target: &Value,
    scope: &ConnectionScope,
    proof: Option<&HeldResponseProof>,
) -> bool {
    validate_response(owner, target, Some(&scope.connection))
        || proof.is_some_and(|proof| {
            proof.matches(owner, scope) && response_identity(owner, &scope.connection) == *target
        })
}

pub enum Claim {
    Replay(Value),
    New {
        record: Value,
        targets: Vec<Value>,
        result: ProcessActionResult,
    },
}
pub fn claim(
    tx: &Transaction<'_>,
    request: &Request,
    connections: &Connections,
    now: f64,
) -> Result<Claim, Error> {
    if let Some(result) = replay(tx, request.raw())? {
        return Ok(Claim::Replay(result));
    }
    let command = request.command();
    let request = request.raw();
    let id = text(request, "requestId", "");
    let signature = digest(request);
    let scope = text(request, "agent", "");
    let owners = owners(tx, scope)?;
    let rows = process_rows(tx, &owners)?;
    let processes = processes(tx, &rows, connections)?;
    let owner = &owners[0];
    if text(owner, "accountKey", "default") != request["accountKey"]
        || owner["epoch"] != request["epoch"]
    {
        return Err(Error::Invalid(
            "The process owner changed. Refresh the process list",
        ));
    }
    let action = text(request, "action", "");
    let mut targets = Vec::new();
    let mut entries = Vec::new();
    let response = if matches!(
        command,
        ProcessControlRequest::StopResponse { .. }
            | ProcessControlRequest::BackgroundCommand { .. }
    ) {
        let valid = if matches!(command, ProcessControlRequest::StopResponse { .. }) {
            !request["responseTarget"].is_null()
                && stop_target(owner, connections) == request["responseTarget"]
        } else {
            validate_response(
                owner,
                &request["responseTarget"],
                connections.get(scope).map(String::as_str),
            )
        };
        if !valid {
            return Err(Error::Invalid(
                "The response changed. Refresh the process list",
            ));
        }
        request["responseTarget"].clone()
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
        for (index, identity) in selected.iter().enumerate() {
            // Keep numeric spellings in identity comparisons as well as the signature.
            let raw_identity = if matches!(command, ProcessControlRequest::StopSelected { .. }) {
                &request["process"]
            } else {
                &request["processes"][index]
            };
            let found = processes.iter().find(|p| matches(p, raw_identity));
            let error = if !seen.insert(&identity.key) {
                Some("A process identity was repeated")
            } else if !include_descendants && identity.agent != scope {
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
                    key: identity.key.clone(),
                    agent: identity.agent.clone(),
                    source: None,
                    generation: None,
                    outcome: ProcessOutcome::Stale {
                        error: error.into(),
                    },
                });
            } else if let Some(process) = found {
                entries.push(ProcessEntry {
                    key: text(process, "key", "").to_owned(),
                    agent: text(process, "agent", "").to_owned(),
                    source: Some(text(process, "source", "").to_owned()),
                    generation: Some(text(process, "generation", "").to_owned()),
                    outcome: ProcessOutcome::Pending,
                });
                targets.push(process.clone());
            }
        }
        Value::Null
    };
    let result = ProcessActionResult {
        request_id: id.into(),
        status: ProcessActionStatus::Uncertain,
        processes: entries,
        response: None,
    };
    let record = json!({"requestId":id,"rootRequestId":studio_runtime_schema::persistence::RequestIdentity::issue_root(),"signature":signature,"phase":"dispatching","scopeAgent":scope,"scopeAccountKey":request["accountKey"],"scopeEpoch":request["epoch"],"action":action,"responseTarget":response,"processes":result.processes,"created":now,"result":result});
    tx.execute(
        "INSERT INTO runtime_process_actions(id,signature,record) VALUES(?,?,?)",
        params![id, signature, canonical(&record)],
    )
    .map_err(StoreError::from)?;
    Ok(Claim::New {
        record,
        targets,
        result,
    })
}
/// Reuse a durable action before any fresh native observation. Changed content
/// under the same request ID remains an error, including after a lost response.
pub fn replay(tx: &Transaction<'_>, request: &Value) -> Result<Option<Value>, Error> {
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT signature,record FROM runtime_process_actions WHERE id=?",
            [text(request, "requestId", "")],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(StoreError::from)?;
    let Some((signature, raw)) = prior else {
        return Ok(None);
    };
    if signature != digest(request) {
        return Err(Error::Invalid(
            "This process action ID has different content",
        ));
    }
    let record: Value = serde_json::from_str(&raw).map_err(StoreError::from)?;
    decode_result(&record).map(Some)
}
pub fn save(tx: &Transaction<'_>, record: &Value) -> Result<(), Error> {
    tx.execute(
        "UPDATE runtime_process_actions SET record=? WHERE id=?",
        params![canonical(record), text(record, "requestId", "")],
    )
    .map_err(StoreError::from)?;
    Ok(())
}
fn decode_result(record: &Value) -> Result<Value, Error> {
    let result = record["result"].clone();
    ProcessActionResult::validate(&result).map_err(StoreError::from)?;
    Ok(result)
}
pub fn receipt(connection: &Connection, query: &Value) -> Result<ProcessReceipt, Error> {
    let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_process_actions')",[],|r|r.get(0)).map_err(StoreError::from)?;
    if !exists {
        return input_receipt(connection, query);
    }
    let record = raw(
        connection,
        "runtime_process_actions",
        text(query, "requestId", ""),
    )?;
    if let Some(record) = record
        && record["scopeAgent"] == query["agent"]
        && record["scopeAccountKey"] == query["accountKey"]
        && record["scopeEpoch"] == query["epoch"]
    {
        return ProcessReceipt::found(record["result"].clone())
            .map_err(StoreError::from)
            .map_err(Error::from);
    }
    input_receipt(connection, query)
}
fn input_receipt(connection: &Connection, query: &Value) -> Result<ProcessReceipt, Error> {
    let value =
        crate::monitor_http_input::read(connection, query).map_err(|error| match error {
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
        })?;
    if value["found"] != true {
        return Ok(ProcessReceipt::not_found());
    }
    ProcessReceipt::found(value["result"].clone())
        .map_err(StoreError::from)
        .map_err(Error::from)
}

#[cfg(test)]
mod claim_identity_tests {
    use super::*;
    use studio_runtime_schema::persistence::RequestIdentity;

    #[test]
    fn process_claim_persists_one_broker_root_without_changing_the_domain_request() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT,parent_id TEXT,deleted_at REAL);
            CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT);
            CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(id);
            CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT);
            CREATE INDEX runtime_task_agent_created_id ON runtime_tasks(id);
            CREATE TABLE runtime_process_actions(id TEXT PRIMARY KEY,signature TEXT,record TEXT);")
            .expect("claim tables");
        let owner = json!({"id":"owner","epoch":3,"accountKey":"account","inFlight":true,
            "threadId":"thread","turnId":"turn"});
        db.execute(
            "INSERT INTO runtime_agents(id,record) VALUES('owner',?1)",
            [owner.to_string()],
        )
        .expect("owner");
        let request = Request::parse(
            json!({"action":"stop_response","requestId":"opaque-action-id",
            "agent":"owner","accountKey":"account","epoch":3,"responseTarget":{
                "agent":"owner","accountKey":"account","epoch":3,"connectionId":"connection",
                "threadId":"thread","turnId":"turn"}}),
        )
        .expect("request");
        let connections = Connections::from([("owner".into(), "connection".into())]);
        let tx = db.transaction().expect("transaction");
        let Claim::New { record, .. } =
            claim(&tx, &request, &connections, 1.0).expect("fresh claim")
        else {
            panic!("first claim must be new");
        };
        let root = record["rootRequestId"]
            .as_str()
            .expect("persisted broker root");
        RequestIdentity::new("process-action", &[], root, &[]).expect("UUIDv7 root");
        assert_eq!(record["requestId"], "opaque-action-id");
        assert!(matches!(
            claim(&tx, &request, &connections, 2.0).expect("replay"),
            Claim::Replay(_)
        ));
        let persisted: String = tx
            .query_row(
                "SELECT record FROM runtime_process_actions WHERE id='opaque-action-id'",
                [],
                |row| row.get(0),
            )
            .expect("owner record");
        assert_eq!(
            serde_json::from_str::<Value>(&persisted).expect("record")["rootRequestId"],
            root
        );
    }
}
