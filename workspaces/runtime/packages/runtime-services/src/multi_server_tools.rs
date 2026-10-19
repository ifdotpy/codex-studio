//! The lead's `orchestration_servers` tool over the owner-paired server channel.
//!
//! Python `MultiServerService.tools` in codex_multi_server_orchestration.py
//! owns these rules, with `codex_project_locations.request` and `suggestions`.
//! Every remote action queues one durable envelope with a stable identity in
//! `runtime_server_outbox` before it crosses the transport. A refusal before
//! that write is a not-applied tool result; the receipt of a sent envelope is
//! the tool result, whatever its outcome.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use futures_util::future::BoxFuture;
use rusqlite::{OptionalExtension, Transaction};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_store::{
    AgentId, AgentRepository, ProjectsRepository, RuntimeStore, StoreError,
};

use crate::event_write::request_signature;
use crate::managed_host_tools::{
    Actor, DeliveryCertainty, DispatchedOperation, Error, OperationResult,
};
use crate::multi_server_auth::AccessError;
use crate::multi_server_orchestration::{ExchangeOutcome, OrchestrationDriver};
use crate::multi_server_transport_limits::MAX_REQUEST_BYTES;
use crate::remote_transport::canonical_json;
use crate::server_git::{self, CHUNK, MAX_BUNDLE, commit_hash, git_command, git_read, git_run};

/// Seconds that `git bundle verify` may run (Python `_fetch`).
const BUNDLE_VERIFY_SECONDS: u64 = 30;
/// Seconds that the local `git fetch` of a bundle may run (Python `_fetch`).
const BUNDLE_FETCH_SECONDS: u64 = 60;
/// Command actions that the executing server owns (Python `_exec_tool`).
const EXEC_ACTIONS: &[&str] = &["exec", "exec_read", "exec_input", "exec_cancel"];
/// Payload fields of a command request (Python `_exec_tool`).
const EXEC_FIELDS: &[&str] = &[
    "cwd",
    "command",
    "env",
    "timeout",
    "output_limit",
    "handle",
    "input",
    "close_stdin",
    "stdout_offset",
    "stderr_offset",
];

/// The tool adapter that managed-tool dispatch calls after admission commits.
pub(crate) trait MultiServerToolPort: Send + Sync {
    fn servers<'a>(
        &'a self,
        actor: &'a Actor,
        args: &'a Value,
        key: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<DispatchedOperation, Error>>;
}

/// Production adapter over the paired-server outbox driver.
pub(crate) struct ServerTools {
    store: Arc<RuntimeStore>,
    driver: Arc<OrchestrationDriver>,
    local_exec: bool,
}

impl ServerTools {
    pub(crate) fn new(
        store: Arc<RuntimeStore>,
        driver: Arc<OrchestrationDriver>,
        local_exec: bool,
    ) -> Self {
        Self {
            store,
            driver,
            local_exec,
        }
    }
}

/// A step result. A refusal proves that no envelope was queued; a failure
/// after a queued envelope or a local Git effect leaves the outcome unknown.
enum Step {
    Refused(String),
    Failed(String),
    Store(StoreError),
}
impl From<StoreError> for Step {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<rusqlite::Error> for Step {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
impl From<crate::project_locations::LocationError> for Step {
    fn from(error: crate::project_locations::LocationError) -> Self {
        match error {
            crate::project_locations::LocationError::Store(error) => Self::Store(error),
            other => Self::Refused(other.to_string()),
        }
    }
}

fn refuse<T>(message: impl Into<String>) -> Result<T, Step> {
    Err(Step::Refused(message.into()))
}

/// A written tool result that carries the paired receipt or local value.
fn written(value: Value) -> DispatchedOperation {
    DispatchedOperation {
        result: OperationResult {
            value,
            signals: Vec::new(),
        },
        certainty: DeliveryCertainty::Written,
        applied: Some(true),
    }
}

fn not_sent(message: String) -> DispatchedOperation {
    DispatchedOperation {
        result: OperationResult {
            value: Value::String(message),
            signals: Vec::new(),
        },
        certainty: DeliveryCertainty::NotSent,
        applied: Some(false),
    }
}

fn settle(step: Result<Value, Step>) -> Result<DispatchedOperation, Error> {
    match step {
        Ok(value) => Ok(written(value)),
        Err(Step::Refused(message)) => Ok(not_sent(message)),
        Err(Step::Failed(message)) => Ok(DispatchedOperation {
            result: OperationResult {
                value: Value::String(message),
                signals: Vec::new(),
            },
            certainty: DeliveryCertainty::Unknown,
            applied: None,
        }),
        Err(Step::Store(error)) => Err(Error::Store(error)),
    }
}

fn io(error: std::io::Error) -> Step {
    Step::Failed(error.to_string())
}

/// Python `identity`.
fn identity(parts: &[&str]) -> String {
    crate::projects::cross_server_identity(parts)
}

/// Python `text_field`: a stripped text of 1 to `maximum` characters.
fn text_field(value: Option<&Value>, name: &str, maximum: usize) -> Result<String, Step> {
    match value.and_then(Value::as_str) {
        Some(text) if text.chars().count() <= maximum && !text.trim().is_empty() => {
            Ok(text.trim().to_owned())
        }
        _ => refuse(format!("Supply {name} with 1 to {maximum} characters")),
    }
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The receipt that the tool returns for one delivered envelope (Python `deliver`).
fn receipt(outcome: &ExchangeOutcome, key: &str) -> Value {
    outcome.to_value(key)
}

impl ServerTools {
    async fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, Step> + Send + 'static,
    ) -> Result<T, Step> {
        let store = Arc::clone(&self.store);
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/multi_server_tools",
            move || store.read_transaction_with_error(operation),
        )
        .await
        .map_err(|_| Step::Store(StoreError::WriterPoisoned))?
    }

    async fn local_server(&self) -> Result<String, Step> {
        let store = Arc::clone(&self.store);
        let local = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/multi_server_tools",
            move || store.local_server_id(),
        )
        .await
        .map_err(|_| Step::Store(StoreError::WriterPoisoned))??;
        match local {
            Some(local) if !local.is_empty() => Ok(local),
            _ => refuse("The local server identity is unavailable"),
        }
    }

    async fn paired_ids(&self) -> Result<Vec<String>, Step> {
        self.read(|tx| {
            Ok(crate::multi_server::paired_servers(tx)?
                .into_iter()
                .filter_map(|server| server["id"].as_str().map(str::to_owned))
                .collect())
        })
        .await
    }

    /// Python `queue` followed by `deliver`: the driver claims the envelope in
    /// the outbox before the transport and records its receipt after it. A
    /// refusal of the claim itself (different content, size) wrote nothing.
    async fn deliver(
        &self,
        server: &str,
        action: &str,
        payload: &Value,
        key: &str,
    ) -> Result<Value, Step> {
        let now = crate::event_write::unix_time()
            .map_err(|_| Step::Refused("The system clock is unavailable".into()))?
            as i64;
        match self
            .driver
            .exchange(server, action, payload, key, now)
            .await
        {
            Ok(outcome) => Ok(receipt(&outcome, key)),
            Err(AccessError::Refused {
                code: "request_conflict" | "request_size" | "invalid_request" | "client_revoked",
                message,
                ..
            }) => {
                if message == "The remote request exceeds 256 KiB" {
                    return refuse("The remote request exceeds 256 KiB; use a smaller batch");
                }
                refuse(message)
            }
            Err(_) => {
                // The claim may have committed. Its saved receipt, when there
                // is one, is the outcome; otherwise the outcome stays unknown.
                let key_for_read = key.to_owned();
                let saved = self
                    .read(move |tx| {
                        Ok(tx
                            .query_row(
                                "SELECT result FROM runtime_server_outbox WHERE id=?1 AND state='complete'",
                                [key_for_read],
                                |row| row.get::<_, Option<String>>(0),
                            )
                            .optional()?
                            .flatten())
                    })
                    .await?;
                Ok(saved
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .unwrap_or_else(|| json!({"requestId": key, "outcome": "unknown"})))
            }
        }
    }

    async fn run(&self, actor: &Actor, args: &Value, key: &str) -> Result<Value, Step> {
        if actor.record().extra.get("isLead") != Some(&Value::Bool(true)) {
            return refuse("Only the lead can use remote server tools");
        }
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let local = self.local_server().await?;
        match action {
            "list" => return self.list(&local).await,
            "add_location" | "remove_location" => {
                return self
                    .location_request(actor, args, action, key, &local)
                    .await;
            }
            "location_matches" => {
                return self
                    .location_matches(args, &identity(&[&actor.record().id.0, action, key]))
                    .await;
            }
            _ if EXEC_ACTIONS.contains(&action) => {
                return self.exec(actor, args, action, key, &local).await;
            }
            _ => {}
        }
        let server = match args.get("server") {
            None if action == "receipt" => Value::String(local.clone()),
            Some(Value::String(server)) if action == "receipt" && server == "local" => {
                Value::String(local.clone())
            }
            None => Value::Null,
            Some(value) => value.clone(),
        };
        let Some(server) = server.as_str() else {
            return refuse("Select a paired server");
        };
        let paired = self.paired_ids().await?;
        if !paired.iter().any(|id| id == server) && !(action == "receipt" && server == local) {
            return refuse("Select a paired server");
        }
        if action == "fetch" {
            return self.fetch(actor, args, key, server).await;
        }
        if !matches!(action, "projects" | "folders" | "git" | "receipt") {
            return refuse("Select list, projects, folders, git, fetch, or receipt");
        }
        if action == "receipt" {
            return self.receipt(actor, args, server).await;
        }
        let mut payload = Map::new();
        for name in ["cwd", "argv"] {
            if let Some(value) = args.get(name) {
                payload.insert(name.to_owned(), value.clone());
            }
        }
        let key = identity(&[&actor.record().id.0, action, key]);
        self.deliver(server, action, &Value::Object(payload), &key)
            .await
    }

    /// Python `tools` action `list`: a server with a failed queued delivery is offline.
    async fn list(&self, local: &str) -> Result<Value, Step> {
        let (servers, pending) = self
            .read(|tx| {
                let servers = crate::multi_server::paired_servers(tx)?;
                let mut statement = tx.prepare(
                    "SELECT DISTINCT server FROM runtime_server_outbox WHERE state='queued' AND error IS NOT NULL",
                )?;
                let pending = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((servers, pending))
            })
            .await?;
        let servers = servers
            .into_iter()
            .map(|mut server| {
                if pending.iter().any(|id| server["id"].as_str() == Some(id)) {
                    server["status"] = json!("offline");
                }
                server
            })
            .collect::<Vec<_>>();
        Ok(json!({"localServer": local, "servers": servers}))
    }

    /// Python `tools` action `receipt`.
    async fn receipt(&self, actor: &Actor, args: &Value, server: &str) -> Result<Value, Step> {
        let Some(request) = args
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return refuse("Supply request_id");
        };
        let actor_id = actor.record().id.0.clone();
        let server = server.to_owned();
        self.read(move |tx| {
            let id = crate::project_locations::tool_receipt_key(tx, &actor_id, &request, &server)?;
            type Row = (String, String, Option<String>, Option<String>);
            let mut row: Option<Row> = tx
                .query_row(
                    "SELECT server,state,result,error,body FROM runtime_server_outbox WHERE id=?1",
                    [&id],
                    |row| {
                        let body: String = row.get(4)?;
                        Ok(((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?), body))
                    },
                )
                .optional()?
                .map(|(row, body)| -> Result<Row, Step> {
                    let envelope: Value = serde_json::from_str(&body)
                        .map_err(|_| Step::Refused("Stored cross-server envelope is invalid".into()))?;
                    if field(&envelope, "action").starts_with("exec")
                        && envelope.pointer("/payload/actor").and_then(Value::as_str)
                            != Some(actor_id.as_str())
                    {
                        return refuse("The command receipt belongs to another lead");
                    }
                    Ok(row)
                })
                .transpose()?;
            if row.is_none() {
                row = tx
                    .query_row(
                        "SELECT server,state,result FROM runtime_server_fetch WHERE id=?1",
                        [&id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, None)),
                    )
                    .optional()?;
            }
            if row.is_none() {
                let retired: Option<Option<String>> = tx
                    .query_row(
                        "SELECT actor FROM runtime_server_retired WHERE id=?1 AND direction='out' AND server=?2",
                        [&id, &server],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(owner) = retired {
                    if owner.is_some_and(|owner| !owner.is_empty() && owner != actor_id) {
                        return refuse("The command receipt belongs to another lead");
                    }
                    return Ok(json!({
                        "state": "expired",
                        "result": expired_receipt(&id),
                        "error": null,
                    }));
                }
            }
            let Some((saved_server, state, result, error)) = row.filter(|row| row.0 == server)
            else {
                return refuse("Unknown remote request receipt");
            };
            let _ = saved_server;
            let result = result
                .map(|raw| {
                    serde_json::from_str::<Value>(&raw)
                        .map(live_receipt)
                        .map_err(|_| Step::Refused("Stored delivery result is invalid".into()))
                })
                .transpose()?;
            Ok(json!({"state": state, "result": result, "error": error}))
        })
        .await
    }

    /// Python `codex_project_locations.request` with the lead as the actor.
    async fn location_request(
        &self,
        actor: &Actor,
        args: &Value,
        action: &str,
        key: &str,
        local: &str,
    ) -> Result<Value, Step> {
        let project_id = text_field(args.get("project"), "a project ID", 4096)?;
        let mut server = text_field(args.get("server"), "a server ID", 128)?;
        if server == "local" {
            server = local.to_owned();
        }
        if server != local && !self.paired_ids().await?.contains(&server) {
            return refuse("The server is not paired or is revoked");
        }
        // Python passes `{**args, 'request_id': key}`: the tool request owns the identity.
        let request_id = text_field(Some(&json!(key)), "a request ID", 128)?;
        let outbox_key = identity(&["project-location", &actor.record().id.0, &request_id]);
        let path = if action == "add_location" {
            Some(text_field(args.get("path"), "a location path", 4096)?)
        } else {
            None
        };
        let signature_data =
            json!({"action": action, "project": project_id, "server": server, "path": path});
        let signature = request_signature(&signature_data);
        let actor_id = actor.record().id.0.clone();
        let epoch = actor.record().epoch;
        let local_for_write = local.to_owned();
        let key_for_write = outbox_key.clone();
        let server_for_write = server.clone();
        let action_for_write = action.to_owned();
        let prepared = self
            .store
            .transaction_with(move |tx| -> Result<Result<Value, Value>, Step> {
                let current = checked_actor(tx, &actor_id)?;
                if current.extra.get("isLead") != Some(&Value::Bool(true)) || current.epoch != epoch {
                    return refuse("Only the active lead can change project locations");
                }
                if let Some((saved, previous)) = ProjectsRepository::receipt(tx, &key_for_write)? {
                    if saved != signature {
                        return refuse("This request id has different content");
                    }
                    return Ok(Err(previous));
                }
                let Some((_, mut project)) = ProjectsRepository::get(tx, &project_id)? else {
                    return refuse("Select an existing project");
                };
                crate::project_locations::normalize_project(&mut project, &local_for_write)?;
                let old: Option<(String, String)> = tx
                    .query_row(
                        "SELECT server,body FROM runtime_server_outbox WHERE id=?1",
                        [&key_for_write],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((old_server, body)) = old {
                    let envelope: Value = serde_json::from_str(&body)
                        .map_err(|_| Step::Refused("Stored cross-server envelope is invalid".into()))?;
                    if old_server != server_for_write || envelope["payload"]["request"] != signature_data {
                        return refuse("This request ID has different content");
                    }
                    return Ok(Ok(envelope["payload"].clone()));
                }
                let busy: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM runtime_server_outbox WHERE state!='complete' AND json_extract(body,'$.action') IN ('add_location','remove_location') AND json_extract(body,'$.payload.project')=?1 LIMIT 1",
                        [&project_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if busy.is_some() {
                    return refuse("A location change is pending. Read its existing receipt");
                }
                let locations = project["locations"].as_array().cloned().unwrap_or_default();
                let location = locations
                    .iter()
                    .find(|row| row["serverId"].as_str() == Some(server_for_write.as_str()))
                    .cloned();
                if action_for_write == "add_location"
                    && let Some(location) = &location
                    && location.get("requestedPath").unwrap_or(&location["path"]) != &json!(path)
                {
                    return refuse("This project already has a location on that server");
                }
                if action_for_write == "remove_location" && location.is_some() && locations.len() == 1 {
                    return refuse("Keep at least one project location");
                }
                let mut payload = json!({
                    "project": project_id,
                    "name": project["name"],
                    "path": path,
                    "request": signature_data,
                    "signature": signature,
                });
                if let Some(location) = location {
                    payload["location"] = location;
                }
                Ok(Ok(payload))
            })
            .await?;
        let payload = match prepared {
            Err(previous) => return Ok(previous),
            Ok(payload) => payload,
        };
        let result = self.deliver(&server, action, &payload, &outbox_key).await?;
        if matches!(result["outcome"].as_str(), Some("applied" | "not_applied")) {
            // Python `applied`: the home location and its receipt commit with
            // the completed outbound receipt.
            let local = local.to_owned();
            let receipt = result.clone();
            let now = crate::event_write::unix_time().unwrap_or_default();
            self.store
                .transaction_with(move |tx| {
                    crate::project_locations::apply_completed_in_transaction(
                        tx,
                        &outbox_key,
                        &server,
                        &receipt,
                        &local,
                        now,
                    )
                    .map_err(Step::from)
                })
                .await?;
        }
        Ok(result)
    }

    /// Python `codex_project_locations.suggestions`: never adds a location.
    async fn location_matches(&self, args: &Value, key: &str) -> Result<Value, Step> {
        let project_id = field(args, "project").to_owned();
        let project = self
            .read(move |tx| Ok(ProjectsRepository::get(tx, &project_id)?.map(|(_, value)| value)))
            .await?;
        let Some(project) = project else {
            return refuse("Select an existing project");
        };
        let server = field(args, "server");
        if !self.paired_ids().await?.iter().any(|id| id == server) {
            return refuse("The server is not paired or is revoked");
        }
        let mut origin = project["locations"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|row| {
                row.get("gitOrigin")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
            })
            .map(str::to_owned);
        if origin.is_none() {
            let path = field(&project, "path").to_owned();
            if !path.starts_with("project:") {
                origin = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-services/multi_server_tools",
                    move || crate::projects::peer_location_info(&path).ok(),
                )
                .await
                .ok()
                .flatten()
                .and_then(|info| info["gitOrigin"].as_str().map(str::to_owned));
            }
        }
        let Some(origin) = origin else {
            return Ok(json!({"matches": []}));
        };
        self.deliver(server, "location_matches", &json!({"origin": origin}), key)
            .await
    }

    /// Python `_exec_tool`.
    async fn exec(
        &self,
        actor: &Actor,
        args: &Value,
        action: &str,
        key: &str,
        local: &str,
    ) -> Result<Value, Step> {
        let server = match args.get("server").and_then(Value::as_str) {
            None | Some("") | Some("local") => local.to_owned(),
            Some(server) => server.to_owned(),
        };
        if server != local && !self.paired_ids().await?.contains(&server) {
            return refuse("Select local or a paired server");
        }
        if server == local && !self.local_exec {
            return refuse("Not implemented: requires multi-server/local-exec");
        }
        let mut payload = Map::new();
        for name in EXEC_FIELDS {
            if let Some(value) = args.get(*name) {
                payload.insert((*name).to_owned(), value.clone());
            }
        }
        payload.insert("actor".to_owned(), json!(actor.record().id.0));
        let mut payload = Value::Object(payload);
        if action == "exec" {
            payload = validate_exec(&payload, server != local).map_err(Step::Refused)?;
        }
        let key = identity(&[&actor.record().id.0, action, key]);
        let encoded =
            canonical_json(&json!({"requestId": key, "action": action, "payload": payload}))
                .map_err(|_| Step::Refused("Invalid cross-server request".into()))?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return refuse("The encoded command request exceeds 256 KiB");
        }
        let actor_id = actor.record().id.0.clone();
        let epoch = actor.record().epoch;
        let handle = payload.get("handle").cloned();
        let server_for_check = server.clone();
        let action_for_check = action.to_owned();
        self.store
            .transaction_with(move |tx| -> Result<(), Step> {
                let current = checked_actor(tx, &actor_id)?;
                if current.extra.get("isLead") != Some(&Value::Bool(true)) || current.epoch != epoch
                {
                    return refuse("Only the active lead can use server commands");
                }
                if action_for_check != "exec" {
                    let Some(handle) = handle.as_ref().filter(|handle| handle.is_string()) else {
                        return refuse("Supply a command handle");
                    };
                    let owned = crate::multi_server_orchestration::exec_handle_owned(
                        tx,
                        &server_for_check,
                        &json!(actor_id),
                        handle,
                    )
                    .map_err(|error| match error {
                        AccessError::Store(error) => Step::Store(error),
                        AccessError::Sqlite(error) => Step::Store(error.into()),
                        other => Step::Refused(other.to_string()),
                    })?;
                    if !owned {
                        return refuse("The command handle belongs to another actor or server");
                    }
                }
                Ok(())
            })
            .await?;
        self.deliver(&server, action, &payload, &key).await
    }

    /// Python `_fetch`: copy a remote worker branch into FETCH_HEAD of a local
    /// repository through a verified Git bundle.
    async fn fetch(
        &self,
        actor: &Actor,
        args: &Value,
        key: &str,
        server: &str,
    ) -> Result<Value, Step> {
        let worker_id = field(args, "agent_id").to_owned();
        let worker = self
            .read(move |tx| {
                Ok(AgentRepository::get(
                    tx,
                    &AgentId::from(worker_id.as_str()),
                )?)
            })
            .await?;
        let Some(worker) = worker else {
            return refuse("Select a remote worker in this team");
        };
        let remote = worker.extra.get("remoteWorker");
        if worker.root_id != actor.record().root_id
            || !studio_runtime_store::truthy(remote)
            || remote
                .and_then(|remote| remote.get("server"))
                .and_then(Value::as_str)
                != Some(server)
        {
            return refuse("Select a remote worker in this team");
        }
        let branch = match args.get("branch").and_then(Value::as_str) {
            Some(branch) if !branch.is_empty() && !branch.starts_with('-') => branch.to_owned(),
            _ => return refuse("Supply a worker branch"),
        };
        let destination = args
            .get("destination")
            .and_then(Value::as_str)
            .or_else(|| actor.record().extra.get("cwd").and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned();
        let branch_for_checks = branch.clone();
        let base = worker
            .extra
            .get("workerBaseCommit")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (destination, known) = blocking(move || -> Result<(String, Vec<String>), Step> {
            let destination = resolve(&destination);
            let path = Path::new(&destination);
            if git(path, &["check-ref-format", "--branch", &branch_for_checks])?.1 != 0 {
                return refuse("Invalid worker branch");
            }
            if git(path, &["rev-parse", "--git-dir"])?.1 != 0 {
                return refuse("Supply an existing local Git repository");
            }
            let mut known = Vec::new();
            for reference in ["HEAD", "origin/main", "FETCH_HEAD"] {
                let object = format!("{reference}^{{commit}}");
                let (output, code) = git(path, &["rev-parse", "--verify", &object])?;
                let commit = String::from_utf8_lossy(&output).trim().to_owned();
                if code == 0 && commit_hash(&commit) {
                    known.push(commit);
                }
            }
            if let Some(base) = base.filter(|base| commit_hash(base)) {
                let object = format!("{base}^{{commit}}");
                if git(path, &["cat-file", "-e", &object])?.1 == 0 {
                    known.push(base);
                }
            }
            Ok((destination, known))
        })
        .await?;
        let signature = request_signature(&json!({
            "worker": worker.id.0,
            "branch": branch,
            "destination": destination,
        }));
        let export_key = identity(&[key, "export"]);
        let key_owned = key.to_owned();
        let server_owned = server.to_owned();
        let signature_for_claim = signature.clone();
        let export_key_for_claim = export_key.clone();
        let worker_cwd = worker.extra.get("cwd").cloned().unwrap_or(Value::Null);
        let mut known_sorted = known;
        known_sorted.sort();
        known_sorted.dedup();
        let branch_for_claim = branch.clone();
        let claim = self
            .store
            .transaction_with(move |tx| -> Result<Result<Value, Value>, Step> {
                if let Some((saved, previous)) = ProjectsRepository::receipt(tx, &key_owned)? {
                    if saved != signature_for_claim {
                        return refuse("This request id has different content");
                    }
                    return Ok(Err(previous));
                }
                let prior: Option<(String, String)> = tx
                    .query_row(
                        "SELECT signature,state FROM runtime_server_fetch WHERE id=?1",
                        [&key_owned],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((saved, state)) = &prior {
                    if *saved != signature_for_claim {
                        return refuse("This request id has different content");
                    }
                    if state == "fetching" {
                        return Ok(Err(json!({
                            "requestId": key_owned,
                            "outcome": "unknown",
                            "detail": "The local Git fetch has no final receipt. Inspect FETCH_HEAD; do not repeat it.",
                        })));
                    }
                }
                tx.execute(
                    "INSERT OR IGNORE INTO runtime_server_fetch VALUES (?1,?2,?3,'preparing',NULL)",
                    [&key_owned, &server_owned, &signature_for_claim],
                )?;
                let prior_export: Option<String> = tx
                    .query_row(
                        "SELECT body FROM runtime_server_outbox WHERE id=?1",
                        [&export_key_for_claim],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(Ok(match prior_export {
                    Some(body) => serde_json::from_str::<Value>(&body)
                        .map_err(|_| Step::Refused("Stored cross-server envelope is invalid".into()))?
                        ["payload"]
                        .clone(),
                    None => json!({"cwd": worker_cwd, "branch": branch_for_claim, "known": known_sorted}),
                }))
            })
            .await?;
        let export = match claim {
            Err(previous) => return Ok(previous),
            Ok(export) => export,
        };
        let receipt = self.deliver(server, "export", &export, &export_key).await?;
        if receipt["outcome"] != "applied" {
            return Ok(receipt);
        }
        let metadata = receipt["value"].clone();
        let already = metadata
            .get("alreadyHave")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(already) = &already {
            let destination = destination.clone();
            let already = already.clone();
            let present = blocking(move || {
                Ok(commit_hash(&already)
                    && git(
                        Path::new(&destination),
                        &["cat-file", "-e", &format!("{already}^{{commit}}")],
                    )?
                    .1 == 0)
            })
            .await?;
            if !present {
                return refuse("The advertised commit is absent from the local repository");
            }
        }
        let bytes = metadata
            .get("bytes")
            .filter(|value| value.is_u64())
            .and_then(Value::as_u64);
        let bytes = match bytes {
            Some(0) if already.is_some() => 0,
            Some(bytes) if bytes > 0 && bytes <= MAX_BUNDLE => bytes,
            _ => return refuse("Invalid remote bundle size"),
        };
        let folder = tempfile::Builder::new()
            .prefix("studio-server-fetch-")
            .tempdir()
            .map_err(io)?;
        let bundle = folder.path().join("worker.bundle");
        if crate::disk_space::free_bytes(folder.path())
            .is_some_and(|free| free < bytes.saturating_mul(2))
        {
            return Err(Step::Failed(
                "Insufficient disk space for the remote Git bundle".into(),
            ));
        }
        let mut digest = Sha256::new();
        let mut data_file = std::fs::File::create(&bundle).map_err(io)?;
        let mut offset: u64 = 0;
        while offset < bytes {
            let chunk_key = identity(&[key, "chunk", &offset.to_string()]);
            let chunk_receipt = self
                .deliver(
                    server,
                    "chunk",
                    &json!({"export": metadata["export"], "offset": offset}),
                    &chunk_key,
                )
                .await?;
            if chunk_receipt["outcome"] != "applied" {
                return Ok(chunk_receipt);
            }
            let chunk = &chunk_receipt["value"];
            let data = chunk["data"]
                .as_str()
                .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
                .unwrap_or_default();
            let length = data.len() as u64;
            if data.is_empty()
                || data.len() > CHUNK
                || chunk["offset"].as_u64() != Some(offset)
                || chunk["nextOffset"].as_u64() != Some(offset + length)
                || offset + length > bytes
                || data_encoding::HEXLOWER.encode(&Sha256::digest(&data)) != field(chunk, "sha256")
            {
                return refuse("Invalid remote bundle chunk");
            }
            use std::io::Write;
            data_file.write_all(&data).map_err(io)?;
            digest.update(&data);
            offset += length;
        }
        data_file.sync_all().map_err(io)?;
        drop(data_file);
        if already.is_none()
            && data_encoding::HEXLOWER.encode(&digest.finalize()) != field(&metadata, "sha256")
        {
            return refuse("The remote bundle checksum differs");
        }
        if already.is_none() {
            let destination = destination.clone();
            let bundle = bundle.clone();
            blocking(move || {
                let command = git_command(
                    Path::new(&destination),
                    &["bundle", "verify", &bundle.to_string_lossy()],
                    false,
                )
                .map_err(git_step)?;
                git_run(&command, BUNDLE_VERIFY_SECONDS).map_err(git_step)
            })
            .await?;
        }
        let key_owned = key.to_owned();
        let claimed = self
            .store
            .transaction_with(move |tx| -> Result<usize, Step> {
                Ok(tx.execute(
                    "UPDATE runtime_server_fetch SET state='fetching' WHERE id=?1 AND state='preparing'",
                    [&key_owned],
                )?)
            })
            .await?;
        if claimed == 0 {
            return Ok(json!({
                "requestId": key,
                "outcome": "unknown",
                "detail": "The original local Git fetch is already reserved.",
            }));
        }
        let source = match &already {
            Some(_) => destination.clone(),
            None => std::fs::canonicalize(&bundle)
                .unwrap_or(bundle.clone())
                .to_string_lossy()
                .into_owned(),
        };
        let reference = already
            .clone()
            .unwrap_or_else(|| format!("refs/heads/{branch}"));
        let destination_for_fetch = destination.clone();
        let commit = blocking(move || {
            let path = Path::new(&destination_for_fetch);
            let command = git_command(path, &["fetch", "--no-tags", &source, &reference], true)
                .map_err(git_step)?;
            git_run(&command, BUNDLE_FETCH_SECONDS).map_err(git_step)?;
            let (output, code) = git(path, &["rev-parse", "FETCH_HEAD"])?;
            let commit = String::from_utf8_lossy(&output).trim().to_owned();
            if code != 0 || !commit_hash(&commit) {
                return Err(Step::Failed(
                    "Git did not return a final fetch commit".into(),
                ));
            }
            Ok(commit)
        })
        .await?;
        drop(folder);
        let value = json!({
            "requestId": key,
            "server": server,
            "branch": branch,
            "commit": commit,
            "bytes": bytes,
            "destination": destination,
        });
        let release_key = identity(&[key, "release"]);
        let key_owned = key.to_owned();
        let value_for_write = value.clone();
        let server_owned = server.to_owned();
        let release = already
            .is_none()
            .then(|| json!({"export": metadata["export"]}));
        let release_for_write = release.clone();
        let release_key_for_write = release_key.clone();
        let now = crate::event_write::unix_time().unwrap_or_default() as i64;
        self.store
            .transaction_with(move |tx| -> Result<(), Step> {
                ProjectsRepository::save_receipt(tx, &key_owned, &signature, &value_for_write)?;
                tx.execute(
                    "UPDATE runtime_server_fetch SET state='complete',result=?1 WHERE id=?2",
                    [
                        &canonical_json(&value_for_write).unwrap_or_default(),
                        &key_owned,
                    ],
                )?;
                if let Some(release) = &release_for_write {
                    crate::multi_server_orchestration::enqueue_in_transaction(
                        tx,
                        &server_owned,
                        "release",
                        release,
                        &release_key_for_write,
                        now,
                    )
                    .map_err(|error| match error {
                        AccessError::Store(error) => Step::Store(error),
                        AccessError::Sqlite(error) => Step::Store(error.into()),
                        other => Step::Refused(other.to_string()),
                    })?;
                }
                Ok(())
            })
            .await?;
        if let Some(release) = release {
            // The receipt above is final. A failed release stays queued in the
            // outbox, and the peer removes an unreleased export after a day.
            if self
                .deliver(server, "release", &release, &release_key)
                .await
                .is_err()
            {}
        }
        Ok(value)
    }
}

impl MultiServerToolPort for ServerTools {
    fn servers<'a>(
        &'a self,
        actor: &'a Actor,
        args: &'a Value,
        key: &'a str,
        _now: f64,
    ) -> BoxFuture<'a, Result<DispatchedOperation, Error>> {
        Box::pin(async move { settle(self.run(actor, args, key).await) })
    }
}

/// Python `checked_actor(db, actor, actor)`.
fn checked_actor(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<studio_runtime_store::AgentRecord, Step> {
    let Some(agent) = AgentRepository::get(tx, &AgentId::from(id))? else {
        return refuse("Unknown agent");
    };
    if studio_runtime_store::truthy(agent.extra.get("deletedAt")) {
        return refuse("This agent was deleted");
    }
    if agent.extra.get("autoWake") != Some(&Value::Bool(true)) {
        return refuse("The caller is stopped");
    }
    Ok(agent)
}

/// Python `expired_receipt`.
fn expired_receipt(key: &str) -> Value {
    json!({
        "requestId": key,
        "outcome": "unknown",
        "expired": true,
        "detail": "The receipt has expired. This identity cannot execute again.",
    })
}

/// Python `live_receipt`: expired command output reads as an expired receipt.
fn live_receipt(result: Value) -> Value {
    let expires = result
        .get("value")
        .filter(|value| value.is_object())
        .and_then(|value| value.get("outputExpiresAt"))
        .and_then(Value::as_f64);
    let now = crate::event_write::unix_time().unwrap_or(f64::MAX);
    if expires.is_some_and(|at| at <= now) {
        return expired_receipt(field(&result, "requestId"));
    }
    result
}

/// Python `Path(destination).expanduser().resolve()`.
fn resolve(path: &str) -> String {
    let expanded = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest.trim_start_matches('/')))
            .unwrap_or_else(|| PathBuf::from(path)),
        _ => PathBuf::from(path),
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded)
    };
    crate::projects::resolve_nonstrict(&absolute)
        .unwrap_or(absolute)
        .to_string_lossy()
        .into_owned()
}

/// A Git failure after the fetch claim leaves its outcome unknown.
fn git_step(error: server_git::GitError) -> Step {
    match error {
        server_git::GitError::Refused(message) => Step::Refused(message),
        server_git::GitError::Failed(message) => Step::Failed(message),
    }
}

/// A Git read before the fetch claim: any failure proves nothing was fetched.
fn git(path: &Path, arguments: &[&str]) -> Result<(Vec<u8>, i32), Step> {
    git_read(path, arguments).map_err(|error| Step::Refused(error.to_string()))
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, Step> + Send + 'static,
) -> Result<T, Step> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/multi_server_tools",
        operation,
    )
    .await
    .map_err(|_| Step::Store(StoreError::WriterPoisoned))?
}

/// Python `codex_server_exec.validate`.
pub(crate) fn validate_exec(
    payload: &Value,
    allow_foreign_windows_path: bool,
) -> Result<Value, String> {
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.contains('\0'));
    let Some(cwd) = cwd else {
        return Err("Supply an absolute cwd on the selected server".into());
    };
    let native_absolute = Path::new(cwd).is_absolute();
    let foreign_absolute = allow_foreign_windows_path && windows_absolute(cwd);
    if !(native_absolute || foreign_absolute) {
        return Err("Supply an absolute cwd on the selected server".into());
    }
    if cwd.split('/').any(|part| part == "..") || cwd.split(['/', '\\']).any(|part| part == "..") {
        return Err("The cwd must not contain parent traversal".into());
    }
    let command = payload.get("command");
    match command {
        Some(Value::String(command)) => {
            if command.is_empty() || command.contains('\0') || command.len() > 128 * 1024 {
                return Err("Supply a command of 1 to 128 KiB without NUL".into());
            }
        }
        Some(Value::Array(argv))
            if (1..=256).contains(&argv.len())
                && argv
                    .iter()
                    .all(|value| value.as_str().is_some_and(|value| !value.contains('\0')))
                && argv[0].as_str().is_some_and(|first| !first.is_empty())
                && argv
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::len)
                    .sum::<usize>()
                    <= 128 * 1024 => {}
        _ => return Err("Supply command argv of 1 to 256 strings, at most 128 KiB".into()),
    }
    let environment = payload.get("env").cloned().unwrap_or_else(|| json!({}));
    let valid_environment = environment.as_object().is_some_and(|object| {
        object.iter().all(|(key, value)| {
            valid_environment_name(key) && value.as_str().is_some_and(|value| !value.contains('\0'))
        })
    }) && crate::event_write::python_json(&environment).len() <= 128 * 1024;
    if !valid_environment {
        return Err(
            "Supply env additions with valid names and string values, at most 128 KiB".into(),
        );
    }
    let timeout = payload.get("timeout").cloned().unwrap_or(json!(120));
    let limit = payload
        .get("output_limit")
        .cloned()
        .unwrap_or(json!(256 * 1024));
    if !timeout
        .as_i64()
        .is_some_and(|value| (1..=1800).contains(&value))
    {
        return Err("Supply timeout seconds from 1 to 1800".into());
    }
    if !limit
        .as_i64()
        .is_some_and(|value| (2..=4 * 1024 * 1024).contains(&value))
    {
        return Err("Supply output_limit bytes from 2 to 4194304".into());
    }
    let mut value = payload.clone();
    value["cwd"] = json!(cwd);
    value["command"] = command.cloned().unwrap_or(Value::Null);
    value["env"] = environment;
    value["timeout"] = timeout;
    value["output_limit"] = limit;
    Ok(value)
}

/// Python `re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', key)`.
fn valid_environment_name(key: &str) -> bool {
    let mut characters = key.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Python `PureWindowsPath(cwd).is_absolute()`: a drive and a root, or a UNC share.
fn windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    let separator = |byte: u8| byte == b'\\' || byte == b'/';
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && separator(bytes[2])
    {
        return true;
    }
    if bytes.len() >= 2 && separator(bytes[0]) && separator(bytes[1]) {
        let rest = &path[2..];
        let mut parts = rest.split(['\\', '/']);
        return matches!((parts.next(), parts.next()), (Some(host), Some(share)) if !host.is_empty() && !share.is_empty());
    }
    false
}

#[cfg(test)]
#[path = "multi_server_tools/tests.rs"]
mod tests;
