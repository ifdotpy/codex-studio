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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
}
pub type Connections = BTreeMap<String, String>;
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
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.to_string(),
    }
}
pub fn digest(value: &Value) -> String {
    studio_runtime_store::sha256_hex(canonical(value).as_bytes())
}
pub fn project(source: &str, raw: &Value, owner: &Value, connection: Option<&str>) -> Value {
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
        if raw["status"] == "running"
            && connection.is_some()
            && raw["reattachedConnectionId"].as_str() == connection
        {
            source_connection = json!(connection);
            source_thread = owner["threadId"].clone();
        }
        if source_thread.as_str().is_none_or(str::is_empty)
            && operation["connectionId"].as_str() == connection
        {
            source_thread = owner["threadId"].clone();
        }
        opaque = text(raw, "id", "").to_owned();
        native = json!([
            raw["id"],
            epoch,
            source_account,
            source_connection,
            source_thread
        ]);
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
    let same = source_account == account
        && epoch == owner["epoch"]
        && connection.is_some_and(|c| !c.is_empty())
        && source_connection.as_str() == connection
        && source_thread.as_str().is_some_and(|v| !v.is_empty())
        && source_thread == owner["threadId"];
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
    connection: &Connection,
    scope: &str,
    connections: &Connections,
) -> Result<(Vec<Value>, Vec<Value>), Error> {
    let owners = owners(connection, scope)?;
    let mut output = Vec::new();
    for table in ["runtime_monitors", "runtime_tasks"] {
        let sql = if table == "runtime_monitors" {
            "SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('approval','starting','running')"
        } else {
            "SELECT record FROM runtime_tasks WHERE json_extract(record,'$.status')='running'"
        };
        let mut statement = connection.prepare(sql).map_err(StoreError::from)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(StoreError::from)?;
        for row in rows {
            let mut raw: Value =
                serde_json::from_str(&row.map_err(StoreError::from)?).map_err(StoreError::from)?;
            let Some(owner) = owners.iter().find(|o| o["id"] == raw["agent"]) else {
                continue;
            };
            let provider = text(
                &raw,
                "sourceProvider",
                text(owner, "provider", ProviderKind::Codex.key()),
            )
            .to_owned();
            let source = if table == "runtime_monitors" {
                "monitor"
            } else if raw["kind"] == "command" && studio_runtime_store::truthy(raw.get("processId"))
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
            if source != "monitor" {
                raw["sourceProvider"] = json!(provider);
            }
            output.push(project(
                source,
                &raw,
                owner,
                connections.get(text(owner, "id", "")).map(String::as_str),
            ));
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
    connection: &Connection,
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
    let command = request.command();
    let request = request.raw();
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
        return Ok(Claim::Replay(decode_result(&record)?));
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
    let response = if matches!(
        command,
        ProcessControlRequest::StopResponse { .. }
            | ProcessControlRequest::BackgroundCommand { .. }
    ) {
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
    let record = json!({"requestId":id,"signature":signature,"phase":"dispatching","scopeAgent":scope,"scopeAccountKey":request["accountKey"],"scopeEpoch":request["epoch"],"action":action,"responseTarget":response,"processes":result.processes,"created":now,"result":result});
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
        return Ok(ProcessReceipt::not_found());
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
    Ok(ProcessReceipt::not_found())
}
