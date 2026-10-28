//! Pure project-location completion and receipt reconciliation, with atomic adapters.
use crate::event_write::{python_json, truthy};
pub use crate::relocation::RelocationError as LocationError;
use crate::relocation::{checked, invalid, timestamp};
use rusqlite::{OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::json_compat::python_json_compact as canonical_json;
pub use studio_runtime_schema::persistence::RemoteOutcome as LocationOutcome;
use studio_runtime_schema::persistence::ServerDeliveryState as OutboxState;
use studio_runtime_store::sha256_hex;
use studio_runtime_store::{
    ProjectLocationsRepository as Repository, ProjectsRepository, RuntimeStore,
};

/// Exact location response shared by HTTP ingress and durable completion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationResult {
    pub request_id: String,
    pub outcome: LocationOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<LocationValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationValue {
    pub server_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub git_origin: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
}

fn present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod receipt_tests {
    use super::*;
    use studio_runtime_schema::persistence::RequestIdentity;
    use studio_runtime_store::request_results::{Admission, RequestResults};

    #[test]
    fn location_reply_preserves_missing_and_null_origin() {
        for value in [
            json!({"serverId":"peer"}),
            json!({"serverId":"peer","gitOrigin":null}),
            json!({"serverId":"peer","gitOrigin":"origin"}),
        ] {
            let parsed: LocationValue =
                serde_json::from_value(value.clone()).expect("location value");
            assert_eq!(json!(parsed), value);
        }
        assert!(
            serde_json::from_value::<LocationResult>(
                json!({"requestId":"request","outcome":"other"})
            )
            .is_err()
        );
    }

    #[test]
    fn hidden_project_completes_pending_location_once() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/relocation/oracle.json")).expect("fixture");
        let case = &fixture["cases"][0];
        let raw = case["outbox"].clone();
        let key = raw["id"].as_str().expect("operation").to_owned();
        let signature = sha256_hex(canonical_json(&raw["body"]["payload"]["request"]).as_bytes());
        let identity = RequestIdentity::new(
            "project-location",
            &["local-fixture", "ui"],
            &RequestIdentity::issue_root(),
            &["location"],
        )
        .expect("identity");
        let now_ms = identity.issued_ms();
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
            CREATE TABLE sync_documents(seq INTEGER);
            CREATE TABLE sync_versions(seq INTEGER);
            CREATE TABLE runtime_orchestration_settings(id TEXT,record TEXT);
            CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,server TEXT,body TEXT,state TEXT,result TEXT,error TEXT,fingerprint TEXT,attempts INTEGER DEFAULT 0,created_at REAL DEFAULT 0,completed_at REAL DEFAULT 0);").expect("schema");
        studio_runtime_store::receipt_schema::create(&db).expect("request schema");
        let tx = db.transaction().expect("transaction");
        let Admission::Fresh(admitted) =
            RequestResults::admit(&tx, &identity, &signature, now_ms).expect("admit")
        else {
            panic!("fresh request")
        };
        ProjectsRepository::put_project(&tx, &case["project"]).expect("project");
        ProjectsRepository::put_location_request(&tx, "/home-project", &key, &admitted)
            .expect("intent");
        tx.execute(
            "INSERT INTO runtime_server_outbox(id,server,body,state) VALUES(?1,'peer',?2,'queued')",
            rusqlite::params![key, raw["body"].to_string()],
        )
        .expect("transport");
        ProjectsRepository::remove(&tx, "/home-project").expect("hide");
        let unknown = json!({"requestId":key,"outcome":"unknown"});
        complete_snapshot_in_transaction(
            &tx,
            &raw,
            &unknown,
            "local-fixture",
            now_ms as f64 / 1000.0,
        )
        .expect("unknown");
        assert!(
            ProjectsRepository::location_request(&tx, "/home-project", &key)
                .expect("intent")
                .is_some()
        );
        complete_snapshot_in_transaction(
            &tx,
            &raw,
            &case["receipt"],
            "local-fixture",
            now_ms as f64 / 1000.0,
        )
        .expect("complete");
        let project = ProjectsRepository::get(&tx, "/home-project")
            .expect("owner")
            .expect("retained")
            .1;
        assert_eq!(project["sidebarHidden"], true);
        assert_eq!(
            project["locations"],
            case["expected"]["project"]["locations"]
        );
        assert!(ProjectsRepository::list(&tx).expect("sidebar").is_empty());
        assert!(
            ProjectsRepository::location_request(&tx, "/home-project", &key)
                .expect("settled intent")
                .is_none()
        );
        let result = crate::event_write::replay_request::<LocationResult>(
            &tx,
            admitted.id(),
            &signature,
            now_ms,
        )
        .expect("replay")
        .expect("reply");
        assert_eq!(json!(result.reply), case["receipt"]);
        assert!(
            complete_snapshot_in_transaction(
                &tx,
                &raw,
                &case["receipt"],
                "local-fixture",
                now_ms as f64 / 1000.0
            )
            .is_err(),
            "the internal callback consumes its token once"
        );
        assert_eq!(
            ProjectsRepository::get(&tx, "/home-project")
                .expect("owner")
                .expect("retained")
                .1,
            project
        );
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
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
    request: Value,
}
#[derive(Deserialize)]
struct Outbox {
    id: String,
    server: String,
    body: Envelope,
    state: OutboxState,
    result: Option<LocationResult>,
}

/// Only this constructor accepts raw persisted rows; fields remain private.
pub struct CompletionSnapshot {
    outbox: Outbox,
    project: Option<Value>,
}
pub struct CompletionDecision {
    project: Option<Value>,
    apply: bool,
}
impl CompletionSnapshot {
    pub fn from_records(outbox: &Value, project: Option<&Value>) -> Result<Self, LocationError> {
        let outbox: Outbox = checked("relocation/LocationOutbox", "runtime_server_outbox", outbox)?;
        if outbox.id.is_empty()
            || outbox.body.request_id.is_empty()
            || outbox.body.payload.request["project"] != outbox.body.payload.project
            || outbox.body.payload.request["server"] != outbox.server
            || outbox.body.payload.request["path"] != json!(outbox.body.payload.path)
            || (outbox.body.payload.request["action"] == "add_location")
                != (outbox.body.action == Action::AddLocation)
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
        receipt: &LocationResult,
        local_server: &str,
        now: f64,
    ) -> Result<CompletionDecision, LocationError> {
        timestamp(now)?;
        if receipt.request_id != self.outbox.body.request_id {
            return Err(LocationError::Refused(
                "The paired server returned a different request identity",
            ));
        }
        if matches!(self.outbox.state, OutboxState::Complete)
            && self.outbox.result.as_ref() != Some(receipt)
        {
            return Err(LocationError::Refused(
                "The completed location receipt has different content",
            ));
        }
        if receipt.outcome == LocationOutcome::Unknown {
            return Ok(CompletionDecision {
                project: None,
                apply: false,
            });
        }
        let applied = receipt.outcome == LocationOutcome::Applied;
        let envelope = &self.outbox.body;
        if applied && envelope.action == Action::AddLocation {
            let location = receipt
                .value
                .as_ref()
                .ok_or_else(|| invalid("location receipt", "/value"))?;
            if location.server_id != self.outbox.server
                || location.home_project_id.as_deref() != Some(envelope.payload.project.as_str())
                || location.path != envelope.payload.path
                || location.request_id.as_deref() != Some(envelope.request_id.as_str())
                || location
                    .canonical_path
                    .as_deref()
                    .is_none_or(|path| !canonical_absolute(path))
                || location.project_id != location.canonical_path
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
                let value = receipt
                    .value
                    .as_ref()
                    .ok_or_else(|| invalid("location receipt", "/value"))?;
                let mut location = json!({"serverId":value.server_id,"projectId":value.project_id,"path":value.canonical_path});
                if let Some(origin) = &value.git_origin {
                    location["gitOrigin"] = json!(origin);
                }
                if value.path != value.canonical_path {
                    location["requestedPath"] = json!(value.path);
                }
                locations.push(location);
            }
            let revision = row
                .get("locationsRevision")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            row["locationsRevision"] = json!(revision + 1);
            row["updated"] = json!(now);
        }
        Ok(CompletionDecision {
            project,
            apply: true,
        })
    }
}

/// The delivery owner calls this with its exact saved envelope and completion
/// proof, then deletes its transport row in this same writer transaction.
pub fn complete_snapshot_in_transaction(
    tx: &Transaction<'_>,
    raw: &Value,
    receipt: &Value,
    local_server: &str,
    now: f64,
) -> Result<Value, LocationError> {
    let project_id = raw
        .pointer("/body/payload/project")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("runtime_server_outbox", "/body/payload/project"))?;
    let project = ProjectsRepository::get(tx, project_id)?;
    let snapshot = CompletionSnapshot::from_records(raw, project.as_ref().map(|(_, value)| value))?;
    let typed: LocationResult =
        serde_json::from_value(receipt.clone()).map_err(studio_runtime_store::StoreError::from)?;
    let decision = snapshot.decide(&typed, local_server, now)?;
    if decision.apply {
        settle_location(
            tx,
            project_id,
            &snapshot.outbox.body.request_id,
            &snapshot.outbox.body.payload.request,
            &typed,
            decision.project,
        )?;
    }
    Ok(receipt.clone())
}

/// Python `normalize` for a caller that holds a project row.
pub(crate) fn normalize_project(
    project: &mut Value,
    local_server: &str,
) -> Result<(), LocationError> {
    normalize(project, local_server)
}

fn settle_location(
    tx: &Transaction<'_>,
    project_id: &str,
    key: &str,
    original_request: &Value,
    receipt: &LocationResult,
    project: Option<Value>,
) -> Result<(), LocationError> {
    let request = ProjectsRepository::location_request(tx, project_id, key)?.ok_or(
        LocationError::Refused("The location request has no admitted project intent"),
    )?;
    if request.signature() != sha256_hex(canonical_json(original_request).as_bytes()) {
        return Err(LocationError::Refused(
            "The location request has different admitted content",
        ));
    }
    let mut reply = receipt.clone();
    let mut project = project
        .or(ProjectsRepository::get(tx, project_id)?.map(|(_, project)| project))
        .ok_or(LocationError::Refused("The project no longer exists"))?;
    if project
        .pointer("/registration/requestId")
        .and_then(Value::as_str)
        == Some(key)
    {
        reply.project_id = Some(project_id.to_owned());
        use studio_runtime_schema::persistence::WorkEffectStatus;
        project["registration"]["status"] = json!(match reply.outcome {
            LocationOutcome::Applied => WorkEffectStatus::Applied,
            LocationOutcome::NotApplied => WorkEffectStatus::Failed,
            LocationOutcome::Unknown =>
                return Err(LocationError::Refused(
                    "The location outcome is still unknown"
                )),
        });
    }
    if let Some(intents) = project
        .get_mut("locationIntents")
        .and_then(Value::as_object_mut)
    {
        intents.remove(key);
    }
    ProjectsRepository::put_project(tx, &project)?;
    crate::event_write::settle_request(tx, &request, None, &reply)
        .map_err(studio_runtime_store::StoreError::from)?;
    Ok(())
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

/// Resolve a managed location request through its current tool owner and exact result.
pub fn tool_location_receipt(
    tx: &Transaction<'_>,
    actor: &str,
    request: &str,
    server: &str,
    now: f64,
) -> Result<Option<LocationResult>, LocationError> {
    let task =
        if let Some(task) = Repository::tool(tx, request)?.filter(|task| task["agent"] == actor) {
            Some(task)
        } else {
            let aliases = Repository::alias_requests(tx, actor, request)?;
            match aliases.as_slice() {
                [] => None,
                [id] => Repository::tool(tx, id)?,
                [_, _, ..] => {
                    return Err(LocationError::Refused(
                        "This request ID names several requests. Use the returned requestId",
                    ));
                }
            }
        };
    let Some(task) = task else {
        return Ok(None);
    };
    let Some((admitted, requested_server)) = task_location_request(tx, &task)? else {
        return Ok(None);
    };
    if requested_server != server {
        return Err(LocationError::Refused(
            "The location request belongs to another server",
        ));
    }
    Ok(crate::event_write::replay_request::<LocationResult>(
        tx,
        admitted.id(),
        admitted.signature(),
        (now * 1000.0) as i64,
    )
    .map_err(studio_runtime_store::StoreError::from)?
    .map(|saved| saved.reply))
}

#[derive(Deserialize)]
struct LocationArguments {
    action: Action,
    project: String,
    server: String,
    path: Option<String>,
}
fn task_location_request(
    tx: &Transaction<'_>,
    task: &Value,
) -> Result<
    Option<(
        studio_runtime_store::request_results::AdmittedRequest,
        String,
    )>,
    LocationError,
> {
    if task["tool"] != "orchestration_servers"
        || !matches!(
            task.pointer("/arguments/action").and_then(Value::as_str),
            Some("add_location" | "remove_location")
        )
    {
        return Ok(None);
    }
    let args: LocationArguments = serde_json::from_value(task["arguments"].clone())
        .map_err(studio_runtime_store::StoreError::from)?;
    let server = if args.server == "local" {
        RuntimeStore::transaction_local_server_id(tx)?.ok_or(LocationError::Refused(
            "The local server identity is unavailable",
        ))?
    } else {
        args.server
    };
    let path = if args.action == Action::AddLocation {
        args.path
    } else {
        None
    };
    let signature = sha256_hex(
        canonical_json(
            &json!({"action":args.action,"project":args.project,"server":server,"path":path}),
        )
        .as_bytes(),
    );
    let parent: studio_runtime_store::request_results::AdmittedRequest =
        serde_json::from_value(task["admitted"].clone())
            .map_err(studio_runtime_store::StoreError::from)?;
    Ok(Some((
        parent.derive_result("project-location", &["location"], &signature),
        server,
    )))
}

/// Settle an uncertain managed tool from its exact completed location result.
pub fn refresh_tool_receipt(
    tx: &Transaction<'_>,
    record: &mut Value,
    now: f64,
) -> Result<bool, LocationError> {
    if record["tool"] != "orchestration_servers"
        || record["outcome"]
            != json!(studio_runtime_schema::persistence::ToolRequestOutcome::Unknown)
    {
        return Ok(false);
    }
    if truthy(record.get("_payloadBlobs")) {
        return Err(LocationError::Requires(
            "payload-store/tool-request-recovery",
        ));
    }
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_tool_requests", "/id"))?
        .to_owned();
    if Repository::tool(tx, &id)?.as_ref() != Some(record) {
        return Err(LocationError::Refused(
            "The tool request changed before location reconciliation",
        ));
    }
    let Some((request, _)) = task_location_request(tx, record)? else {
        return Ok(false);
    };
    let Some(receipt) = crate::event_write::replay_request::<LocationResult>(
        tx,
        request.id(),
        request.signature(),
        (now * 1000.0) as i64,
    )
    .map_err(studio_runtime_store::StoreError::from)?
    else {
        return Ok(false);
    };
    if receipt.reply.outcome == LocationOutcome::Unknown {
        return Ok(false);
    }
    let result = json!({"success":true,"contentItems":[{"type":"inputText","text":python_json(&json!(receipt.reply))},{"type":"inputText","text":format!("[Time awareness] Tool result finalized at {}",timestamp(now)?)}]});
    /// Maximum inline recovered location tool result. Unit: UTF-8 bytes.
    const MAX_LOCATION_TOOL_RESULT_BYTES: usize = 64 * 1024;
    if result["contentItems"].to_string().len() > MAX_LOCATION_TOOL_RESULT_BYTES {
        return Err(LocationError::Requires(
            "payload-store/tool-request-recovery",
        ));
    }
    let mut next = record.clone();
    next["result"] = result.clone();
    next["outcome"] = json!(receipt.reply.outcome);
    next["stage"] = json!(match receipt.reply.outcome {
        LocationOutcome::Applied =>
            studio_runtime_schema::persistence::ToolRequestStatus::Completed,
        LocationOutcome::NotApplied =>
            studio_runtime_schema::persistence::ToolRequestStatus::Failed,
        LocationOutcome::Unknown => return Ok(false),
    });
    next["updated"] = json!(now);
    Repository::save_tool(tx, &id, &next, &result)?;
    *record = next;
    Ok(true)
}

/// Internal outbound payload field. Its value binds reservation to the checked
/// local project sequence, workspace, and optional managed actor epoch.
pub const LOCATION_ADMISSION_FIELD: &str = "_projectAdmission";
#[derive(serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocationAdmission {
    project: String,
    seq: i64,
    workspace: String,
    actor: Option<String>,
    epoch: Option<i64>,
}
pub fn location_admission(
    db: &rusqlite::Connection,
    project: &str,
    actor: Option<(&str, i64)>,
) -> Result<Value, LocationError> {
    let seq=db.query_row(
        "SELECT s.seq FROM sync_entities s JOIN runtime_projects p ON p.id=s.id WHERE s.collection='project' AND s.id=?1 AND s.deleted=0",
        [project],|row|row.get::<_,i64>(0),
    ).optional()?.ok_or(LocationError::Refused("The project changed before location reservation"))?;
    serde_json::to_value(LocationAdmission {
        project: project.into(),
        seq,
        workspace: studio_runtime_store::SyncDocumentsRepository::workspace_id(db)?,
        actor: actor.map(|(id, _)| id.into()),
        epoch: actor.map(|(_, epoch)| epoch),
    })
    .map_err(|_| LocationError::Refused("Invalid project location admission"))
}
pub(crate) fn check_location_admission(
    tx: &Transaction<'_>,
    payload: &Value,
) -> Result<(), LocationError> {
    let saved = payload
        .get(LOCATION_ADMISSION_FIELD)
        .ok_or(LocationError::Refused(
            "A checked project location admission is required",
        ))?;
    let expected: LocationAdmission = serde_json::from_value(saved.clone())
        .map_err(|_| LocationError::Refused("Invalid project location admission"))?;
    if payload.get("project").and_then(Value::as_str) != Some(expected.project.as_str()) {
        return Err(LocationError::Refused(
            "The project location admission changed",
        ));
    }
    let current = location_admission(
        tx,
        &expected.project,
        expected.actor.as_deref().zip(expected.epoch),
    )?;
    if current != *saved {
        return Err(LocationError::Refused(
            "The project changed before location reservation",
        ));
    }
    if let Some(actor) = expected.actor {
        let current = ProjectsRepository::agent(tx, &actor)?
            .ok_or(LocationError::Refused("The caller is no longer available"))?
            .1;
        if current.get("epoch").and_then(Value::as_i64) != expected.epoch
            || truthy(current.get("deletedAt"))
            || current.get("autoWake") != Some(&Value::Bool(true))
        {
            return Err(LocationError::Refused(
                "The caller changed before location reservation",
            ));
        }
    }
    Ok(())
}
