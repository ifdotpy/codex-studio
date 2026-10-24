//! Process identities and durable action receipts shared with the Python backend.
//! Listing is read-only; native effects are driven only after an action is claimed.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
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
}
impl FromIterator<(String, String)> for Connections {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self {
            owners: iter.into_iter().collect(),
            sources: BTreeMap::new(),
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
    owners: &[Value],
) -> Result<Vec<ConnectionScope>, Error> {
    let mut scopes = BTreeMap::new();
    for (_, raw) in studio_runtime_store::StartupRecoveryRepository::active_monitors(tx)? {
        let monitor: Value = serde_json::from_str(&raw).map_err(StoreError::from)?;
        if let Some(owner) = owners.iter().find(|owner| owner["id"] == monitor["agent"])
            && let Some(scope) = retained_monitor_scope(tx, &monitor, owner)?
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
pub fn owners(connection: &Connection, scope: &str) -> Result<Vec<Value>, Error> {
    let root =
        raw(connection, "runtime_agents", scope)?.ok_or(Error::Invalid("Unknown process owner"))?;
    if studio_runtime_store::truthy(root.get("deletedAt")) {
        return Err(Error::Invalid("This process owner was deleted"));
    }
    let mut statement = connection
        .prepare(
            "SELECT record FROM runtime_agents WHERE json_extract(record,'$.deletedAt') IS NULL",
        )
        .map_err(StoreError::from)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(StoreError::from)?;
    let mut children: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in rows {
        let value: Value =
            serde_json::from_str(&row.map_err(StoreError::from)?).map_err(StoreError::from)?;
        if let Some(parent) = value["parentId"].as_str() {
            children.entry(parent.to_owned()).or_default().push(value);
        }
    }
    let mut result = vec![root];
    let mut seen = HashSet::from([scope.to_owned()]);
    let mut index = 0;
    while index < result.len() {
        let id = result[index]["id"]
            .as_str()
            .ok_or(Error::Invalid("Invalid process owner"))?;
        if let Some(children) = children.remove(id) {
            for child in children {
                if let Some(id) = child["id"].as_str()
                    && seen.insert(id.to_owned())
                {
                    result.push(child);
                }
            }
        }
        index += 1;
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
        _ => value.to_string(),
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
    let native;
    let pending =
        source == "monitor" && matches!(raw["status"].as_str(), Some("starting" | "approval"));
    let is_monitor = source == "monitor";
    let is_claude = source == "claude_task";
    if is_monitor {
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
        native = identity;
    } else {
        epoch = raw["ownerEpoch"].clone();
        let item = text(raw, "itemId", "");
        opaque = if is_claude {
            item.strip_prefix("task:").unwrap_or(item).to_owned()
        } else {
            text(raw, "processId", item).to_owned()
        };
        native = if is_claude {
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
    }
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
    scope: &str,
    connections: &Connections,
) -> Result<(Vec<Value>, Vec<Value>), Error> {
    let owners = owners(connection, scope)?;
    let mut output = Vec::new();
    for table in ["runtime_monitors", "runtime_tasks"] {
        let rows = if table == "runtime_monitors" {
            studio_runtime_store::StartupRecoveryRepository::active_monitors(connection)?
                .into_iter()
                .map(|(_, raw)| raw)
                .collect::<Vec<_>>()
        } else {
            let mut statement=connection.prepare("SELECT record FROM runtime_tasks WHERE json_extract(record,'$.status')='running'").map_err(StoreError::from)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(StoreError::from)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)?
        };
        for row in rows {
            let mut raw: Value = serde_json::from_str(&row).map_err(StoreError::from)?;
            let Some(owner) = owners.iter().find(|o| o["id"] == raw["agent"]) else {
                continue;
            };
            let provider =
                text(&raw, "sourceProvider", text(owner, "provider", "codex")).to_owned();
            let source = if table == "runtime_monitors" {
                "monitor"
            } else if raw["kind"] == "command" && studio_runtime_store::truthy(raw.get("processId"))
            {
                "codex_task"
            } else if provider == "claude"
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
    }
    output.sort_by(|a, b| {
        b["created"]
            .as_f64()
            .partial_cmp(&a["created"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| text(a, "key", "").cmp(text(b, "key", "")))
    });
    Ok((owners, output))
}
pub fn response_target(owner: &Value, connection: Option<&str>) -> Value {
    if owner["inFlight"] == true
        && connection.is_some()
        && !text(owner, "threadId", "").is_empty()
        && !text(owner, "turnId", "").is_empty()
    {
        json!({"agent":owner["id"],"accountKey":text(owner,"accountKey","default"),"epoch":owner["epoch"],"connectionId":connection,"threadId":owner["threadId"],"turnId":owner["turnId"]})
    } else {
        Value::Null
    }
}
pub fn listing(
    connection: &Transaction<'_>,
    scope: &str,
    connections: &Connections,
) -> Result<Value, Error> {
    let (owners, mut processes) = processes(connection, scope, connections)?;
    for p in &mut processes {
        p.as_object_mut()
            .expect("process object")
            .retain(|k, _| !k.starts_with('_'));
    }
    Ok(
        json!({"responseTarget":response_target(&owners[0],connections.get(scope).map(String::as_str)),"processes":processes}),
    )
}
pub fn matches(process: &Value, identity: &Value) -> bool {
    ["key", "generation", "agent", "accountKey", "epoch"]
        .iter()
        .all(|k| process[k] == identity[k])
}
pub fn validate_response(owner: &Value, target: &Value, connection: Option<&str>) -> bool {
    !target.is_null() && response_target(owner, connection) == *target
}

pub enum Claim {
    Replay(Value),
    New { record: Value, targets: Vec<Value> },
}
pub fn claim(
    tx: &Transaction<'_>,
    request: &Value,
    connections: &Connections,
    now: f64,
) -> Result<Claim, Error> {
    let id = text(request, "requestId", "");
    let signature = digest(request);
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT signature,record FROM runtime_process_actions WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(StoreError::from)?;
    if let Some((old, raw)) = prior {
        if old != signature {
            return Err(Error::Invalid(
                "This process action ID has different content",
            ));
        }
        let record: Value = serde_json::from_str(&raw).map_err(StoreError::from)?;
        return Ok(Claim::Replay(record["result"].clone()));
    }
    let scope = text(request, "agent", "");
    let (owners, processes) = processes(tx, scope, connections)?;
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
    let response = if matches!(action, "stop_response" | "background_command") {
        if !validate_response(
            owner,
            &request["responseTarget"],
            connections.get(scope).map(String::as_str),
        ) {
            return Err(Error::Invalid(
                "The response changed. Refresh the process list",
            ));
        }
        request["responseTarget"].clone()
    } else {
        let selected = if action == "stop_selected" {
            vec![request["process"].clone()]
        } else {
            request["processes"].as_array().cloned().unwrap_or_default()
        };
        let mut seen = HashSet::new();
        for identity in selected {
            let found = processes.iter().find(|p| matches(p, &identity));
            let error = if !seen.insert(identity["key"].clone().to_string()) {
                Some("A process identity was repeated")
            } else if request["includeDescendants"] != true && identity["agent"] != scope {
                Some("Worker processes are outside this stop scope")
            } else if found.is_none() {
                Some("This process changed or finished. Refresh the process list")
            } else if found.is_some_and(|p| p["capabilities"]["stop"] != true) {
                Some("The process owner changed. Refresh the process list")
            } else {
                None
            };
            if let Some(error) = error {
                entries.push(json!({"key":identity["key"],"agent":identity["agent"],"status":"stale","error":error}));
            } else if let Some(process) = found {
                entries.push(json!({"key":process["key"],"agent":process["agent"],"source":process["source"],"generation":process["generation"],"status":"pending"}));
                targets.push(process.clone());
            }
        }
        Value::Null
    };
    let result = json!({"requestId":id,"status":"uncertain","processes":entries});
    let record = json!({"requestId":id,"signature":signature,"phase":"dispatching","scopeAgent":scope,"scopeAccountKey":request["accountKey"],"scopeEpoch":request["epoch"],"action":action,"responseTarget":response,"processes":entries,"created":now,"result":result});
    tx.execute(
        "INSERT INTO runtime_process_actions(id,signature,record) VALUES(?,?,?)",
        params![id, signature, canonical(&record)],
    )
    .map_err(StoreError::from)?;
    Ok(Claim::New { record, targets })
}
pub fn save(tx: &Transaction<'_>, record: &Value) -> Result<(), Error> {
    tx.execute(
        "UPDATE runtime_process_actions SET record=? WHERE id=?",
        params![canonical(record), text(record, "requestId", "")],
    )
    .map_err(StoreError::from)?;
    Ok(())
}
pub fn receipt(connection: &Connection, query: &Value) -> Result<Value, Error> {
    let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_process_actions')",[],|r|r.get(0)).map_err(StoreError::from)?;
    if !exists {
        return Ok(json!({"found":false,"result":null}));
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
        return Ok(json!({"found":true,"result":record["result"]}));
    }
    Ok(json!({"found":false,"result":null}))
}
