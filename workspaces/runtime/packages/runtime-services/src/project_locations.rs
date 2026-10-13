//! Pure project-location completion and receipt reconciliation, with atomic adapters.
use crate::event_write::{python_json, request_signature, truthy};
pub use crate::relocation::RelocationError as LocationError;
use crate::relocation::{checked, identity, invalid, timestamp};
use rusqlite::Transaction;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use studio_runtime_store::{
    ProjectLocationsRepository as Repository, ProjectsRepository, RuntimeStore,
};

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Action {
    AddLocation,
    RemoveLocation,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    request_id: String,
    action: Action,
    payload: Payload,
}
#[derive(Deserialize)]
struct Payload {
    project: String,
    path: Option<String>,
    signature: String,
    request: Value,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum OutboxState {
    Queued,
    Complete,
}
#[derive(Deserialize)]
struct Outbox {
    id: String,
    server: String,
    body: Envelope,
    state: OutboxState,
    result: Option<Value>,
}

/// Only this constructor accepts raw persisted rows; fields remain private.
pub struct CompletionSnapshot {
    outbox: Outbox,
    project: Option<Value>,
}
pub struct CompletionDecision {
    project: Option<Value>,
    receipt: Value,
    apply: bool,
}
impl CompletionSnapshot {
    pub fn from_records(outbox: &Value, project: Option<&Value>) -> Result<Self, LocationError> {
        let outbox: Outbox = checked("relocation/LocationOutbox", "runtime_server_outbox", outbox)?;
        if outbox.id != outbox.body.request_id
            || outbox.body.payload.request["project"] != outbox.body.payload.project
            || outbox.body.payload.request["server"] != outbox.server
            || outbox.body.payload.request["path"] != json!(outbox.body.payload.path)
            || (outbox.body.payload.request["action"] == "add_location")
                != (outbox.body.action == Action::AddLocation)
            || request_signature(&outbox.body.payload.request) != outbox.body.payload.signature
        {
            return Err(invalid("runtime_server_outbox", "body/payload/request"));
        }
        let project = project
            .map(|row| checked::<Value>("relocation/RelocationProject", "runtime_projects", row))
            .transpose()?;
        if project
            .as_ref()
            .is_some_and(|row| row["id"] != outbox.body.payload.project)
        {
            return Err(invalid("runtime_projects", "/id"));
        }
        Ok(Self { outbox, project })
    }
    pub fn decide(
        &self,
        receipt: &Value,
        local_server: &str,
        now: f64,
    ) -> Result<CompletionDecision, LocationError> {
        timestamp(now)?;
        checked::<Value>("relocation/RelocationReceipt", "location receipt", receipt)?;
        if receipt["requestId"] != self.outbox.id {
            return Err(LocationError::Refused(
                "The paired server returned a different request identity",
            ));
        }
        if matches!(self.outbox.state, OutboxState::Complete) {
            let prior = self
                .outbox
                .result
                .as_ref()
                .ok_or_else(|| invalid("runtime_server_outbox", "/result"))?;
            if prior != receipt {
                return Err(LocationError::Refused(
                    "The completed location receipt has different content",
                ));
            }
            return Ok(CompletionDecision {
                project: None,
                receipt: prior.clone(),
                apply: false,
            });
        }
        if receipt["outcome"] == "unknown" {
            return Ok(CompletionDecision {
                project: None,
                receipt: receipt.clone(),
                apply: false,
            });
        }
        let applied = receipt["outcome"] == "applied";
        let envelope = &self.outbox.body;
        if applied && envelope.action == Action::AddLocation {
            let location = &receipt["value"];
            let canonical = location.get("canonicalPath").and_then(Value::as_str);
            if location["serverId"] != self.outbox.server
                || location["homeProjectId"] != envelope.payload.project
                || location["path"] != json!(envelope.payload.path)
                || location["requestId"] != self.outbox.id
                || canonical.is_none_or(|path| !canonical_absolute(path))
                || location["projectId"] != location["canonicalPath"]
            {
                return Err(LocationError::Refused(
                    "The server returned a different project location",
                ));
            }
        }
        let mut project = self.project.clone().filter(|_| applied);
        if let Some(row) = project.as_mut() {
            normalize(row, local_server)?;
            let locations = row["locations"]
                .as_array_mut()
                .ok_or_else(|| invalid("runtime_projects", "/locations"))?;
            locations.retain(|location| location["serverId"] != self.outbox.server);
            if envelope.action == Action::AddLocation {
                let value = &receipt["value"];
                let mut location = json!({"serverId":value["serverId"],"projectId":value["projectId"],"path":value["canonicalPath"]});
                if let Some(origin) = value.get("gitOrigin") {
                    location["gitOrigin"] = origin.clone();
                }
                if value["path"] != value["canonicalPath"] {
                    location["requestedPath"] = value["path"].clone();
                }
                locations.push(location);
            }
            let revision = row
                .get("locationsRevision")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            row["locationsRevision"] = json!(revision + 1);
            row["updated"] = json!(now);
            checked::<Value>("relocation/RelocationProject", "runtime_projects", row)?;
        }
        Ok(CompletionDecision {
            project,
            receipt: receipt.clone(),
            apply: true,
        })
    }
}

/// Authenticated paired-delivery caller supplies the exact saved envelope. No network I/O.
/// Completion, project revision, sync projection and receipts share one store commit.
pub fn complete(
    store: &RuntimeStore,
    key: &str,
    server: &str,
    envelope: &Value,
    receipt: &Value,
    local_server: &str,
    now: f64,
) -> Result<Value, LocationError> {
    store.transaction_with(|tx| {
        complete_in_transaction(tx, key, server, envelope, receipt, local_server, now)
    })
}
pub fn complete_in_transaction(
    tx: &Transaction<'_>,
    key: &str,
    server: &str,
    envelope: &Value,
    receipt: &Value,
    local_server: &str,
    now: f64,
) -> Result<Value, LocationError> {
    let raw =
        Repository::outbox(tx, key)?.ok_or(LocationError::Refused("Unknown location request"))?;
    if raw["server"] != server || raw["body"] != *envelope {
        return Err(LocationError::Refused(
            "The location transport ticket has different content",
        ));
    }
    let project_id = raw
        .pointer("/body/payload/project")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("runtime_server_outbox", "/body/payload/project"))?;
    let project = ProjectsRepository::get(tx, project_id)?;
    let snapshot =
        CompletionSnapshot::from_records(&raw, project.as_ref().map(|(_, value)| value))?;
    let decision = snapshot.decide(receipt, local_server, now)?;
    if decision.apply {
        if receipt["outcome"] == "applied" {
            let signature = &snapshot.outbox.body.payload.signature;
            if let Some((prior_signature, prior)) = ProjectsRepository::receipt(tx, key)? {
                if prior_signature != *signature || prior != *receipt {
                    return Err(LocationError::Refused(
                        "This request ID has different content",
                    ));
                }
            } else {
                ProjectsRepository::save_receipt(tx, key, signature, receipt)?;
            }
            Repository::registration_receipt(tx, project_id, key, receipt)?;
            if let Some(project) = decision.project {
                ProjectsRepository::put_project(tx, &project)?;
            }
        }
        Repository::complete(tx, key, receipt, now)?;
    }
    Ok(decision.receipt)
}

fn normalize(project: &mut Value, local_server: &str) -> Result<(), LocationError> {
    if project.get("locations").is_none() {
        project["homeServerId"] = json!(local_server);
        project["locations"] =
            json!([{"serverId":local_server,"path":project["path"],"projectId":project["id"]}]);
        project["locationsRevision"] = json!(0);
    }
    checked::<Value>("relocation/RelocationProject", "runtime_projects", project)?;
    Ok(())
}

// POSIX normpath's canonical absolute spellings, including the special leading //.
fn canonical_absolute(path: &str) -> bool {
    if !path.starts_with('/') || path.contains('\0') {
        return false;
    }
    let tail = path.strip_prefix("//").unwrap_or(&path[1..]);
    tail.is_empty()
        || tail
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Resolve caller aliases to a saved wire identity, refusing ambiguous aliases.
pub fn tool_receipt_key(
    tx: &Transaction<'_>,
    actor: &str,
    request: &str,
    server: &str,
) -> Result<String, LocationError> {
    let aliases = Repository::alias_requests(tx, actor, request)?;
    if aliases.len() > 1000 {
        return Err(LocationError::Refused("Too many location receipt aliases"));
    }
    let candidates = aliases
        .iter()
        .map(|alias| identity(&["project-location", actor, alias]))
        .chain(std::iter::once(identity(&[
            "project-location",
            actor,
            request,
        ])))
        .collect::<BTreeSet<_>>();
    let mut matches = Vec::new();
    for candidate in candidates {
        let outbox = Repository::outbox(tx, &candidate)?;
        let found = outbox.as_ref().is_some_and(|row| {
            row["server"] == server
                && matches!(
                    row.pointer("/body/action").and_then(Value::as_str),
                    Some("add_location" | "remove_location")
                )
        });
        if found || Repository::retired(tx, &candidate, server)? {
            matches.push(candidate);
        }
    }
    match matches.len() {
        0 => Ok(request.to_owned()),
        1 => Ok(matches.remove(0)),
        _ => Err(LocationError::Refused(
            "This request ID names several location receipts. Use the returned requestId",
        )),
    }
}

/// The tool-request consumer passes its exact record read under this transaction.
/// Bind all durable identity fields by comparing it with the stored row before repairing it.
pub fn refresh_tool_receipt(
    tx: &Transaction<'_>,
    record: &mut Value,
    now: f64,
) -> Result<bool, LocationError> {
    if record.get("tool").and_then(Value::as_str) != Some("orchestration_servers") {
        return Ok(false);
    }
    if truthy(record.get("_payloadBlobs")) {
        return Err(LocationError::Requires(
            "payload-store/tool-request-recovery",
        ));
    }
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("runtime_tool_requests", "/id"))?
        .to_owned();
    if Repository::tool(tx, &id)?.as_ref() != Some(record) {
        return Err(LocationError::Refused(
            "The tool request changed before location reconciliation",
        ));
    }
    checked::<Value>("relocation/ToolRequest", "runtime_tool_requests", record)?;
    let saved = Repository::tool_result(tx, &id)?;
    let actor = record["agent"]
        .as_str()
        .ok_or_else(|| invalid("runtime_tool_requests", "/agent"))?;
    let key = identity(&["project-location", actor, &id]);
    let outbox = Repository::outbox(tx, &key)?;
    let Some((next, result)) = reconcile_tool(record, saved.as_ref(), outbox.as_ref(), now)? else {
        return Ok(false);
    };
    Repository::save_tool(tx, &id, &next, &result)?;
    *record = next;
    Ok(true)
}

/// Pure repair decision over checked saved records; no wall clock or transport access.
fn reconcile_tool(
    record: &Value,
    saved: Option<&Value>,
    raw: Option<&Value>,
    now: f64,
) -> Result<Option<(Value, Value)>, LocationError> {
    let previous = saved
        .filter(|v| truthy(Some(v)))
        .or_else(|| record.get("result"));
    let Some(previous) = previous.filter(|v| v["success"] == true) else {
        return Ok(None);
    };
    if truthy(previous.get("_payloadBlobs")) {
        return Err(LocationError::Requires(
            "payload-store/tool-request-recovery",
        ));
    }
    let Some(text) = previous
        .pointer("/contentItems/0/text")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Ok(None);
    };
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_tool_requests", "/id"))?;
    let actor = record
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("runtime_tool_requests", "/agent"))?;
    let key = identity(&["project-location", actor, id]);
    if value["requestId"] != key || value["outcome"] != "unknown" {
        return Ok(None);
    }
    let Some(raw) = raw else {
        return Ok(None);
    };
    let snapshot = CompletionSnapshot::from_records(raw, None)?;
    if !matches!(snapshot.outbox.state, OutboxState::Complete) {
        return Ok(None);
    }
    let Some(receipt) = snapshot.outbox.result else {
        return Ok(None);
    };
    if receipt["requestId"] != key
        || !matches!(receipt["outcome"].as_str(), Some("applied" | "not_applied"))
    {
        return Ok(None);
    }
    let result = json!({"success":true,"contentItems":[{"type":"inputText","text":python_json(&receipt)},{"type":"inputText","text":format!("[Time awareness] Tool result finalized at {}",timestamp(now)?)}]});
    if result["contentItems"].to_string().len() > 64 * 1024 {
        return Err(LocationError::Requires(
            "payload-store/tool-request-recovery",
        ));
    }
    let mut next = record.clone();
    next["result"] = result.clone();
    next["outcome"] = receipt["outcome"].clone();
    next["stage"] = json!(if receipt["outcome"] == "applied" {
        "completed"
    } else {
        "failed"
    });
    next["updated"] = json!(now);
    Ok(Some((next, result)))
}
