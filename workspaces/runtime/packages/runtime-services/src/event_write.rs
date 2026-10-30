//! Atomic annotation writes with durable scheduler events.

use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
pub(crate) use studio_runtime_process::clock::monotonic_ns;

use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_schema::annotation_command::AnnotationData;
use studio_runtime_schema::delivery::DeliveryMoment;
use studio_runtime_schema::persistence::{RequestIdentity, RequestReferenceKind};
use studio_runtime_store::request_results::{
    Admission, AdmittedRequest, RequestReference, RequestResultError, RequestResults,
    ResultEnvelope,
};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, EventWriteRepository, QueueEvent,
    QueueEventId, QueueWriteRepository, Transaction,
};
use thiserror::Error;

pub use studio_runtime_store::{
    python_json, python_json_compact, python_json_with_separators, truthy,
};

pub fn request_signature(value: &Value) -> String {
    studio_runtime_store::sha256_hex(python_json(value).as_bytes())
}

/// Authorization belongs to the caller and must precede replay disclosure.
pub fn replay_request<R: serde::de::DeserializeOwned>(
    connection: &studio_runtime_store::Connection,
    id: &str,
    signature: &str,
    now_ms: i64,
) -> Result<Option<ResultEnvelope<R>>, RequestResultError> {
    RequestResults::replay(connection, id, signature, now_ms)?
        .map(|stored| serde_json::from_str(&stored.result).map_err(Into::into))
        .transpose()
}

pub fn settle_request<R: serde::Serialize>(
    transaction: &Transaction<'_>,
    admitted: &AdmittedRequest,
    actor: Option<&str>,
    reply: &R,
) -> Result<(), RequestResultError> {
    RequestResults::settle(
        transaction,
        admitted,
        &ResultEnvelope {
            actor: actor.map(str::to_owned),
            reply,
            references: Vec::new(),
        },
    )?;
    Ok(())
}

pub enum CallerAdmission<R> {
    Fresh(AdmittedRequest),
    Replay(R),
}

#[derive(Clone, Copy)]
pub struct RequestKey<'a> {
    pub raw: &'a str,
    pub namespace: &'a str,
    pub authority: &'a [&'a str],
    pub root: &'a str,
    pub operation: &'a [&'a str],
}
impl<'a> RequestKey<'a> {
    pub fn new(namespace: &'a str, authority: &'a [&'a str], root: &'a str) -> Self {
        Self {
            raw: root,
            namespace,
            authority,
            root,
            operation: &[],
        }
    }
    pub fn raw(mut self, raw: &'a str) -> Self {
        self.raw = raw;
        self
    }
    pub fn operation(mut self, operation: &'a [&'a str]) -> Self {
        self.operation = operation;
        self
    }
}

/// The common public-caller flow: authorized exact lookup, then UUIDv7 admission.
pub fn begin_request<R: serde::de::DeserializeOwned>(
    transaction: &Transaction<'_>,
    key: RequestKey<'_>,
    signature: &str,
    now_ms: i64,
) -> Result<CallerAdmission<R>, RequestResultError> {
    if let Some(saved) = replay_request(transaction, key.raw, signature, now_ms)? {
        return Ok(CallerAdmission::Replay(saved.reply));
    }
    Ok(
        match RequestResults::admit(transaction, &request_identity(&key)?, signature, now_ms)? {
            Admission::Fresh(admitted) => CallerAdmission::Fresh(admitted),
            Admission::Replay(saved) => {
                let saved: ResultEnvelope<R> = serde_json::from_str(&saved.result)?;
                CallerAdmission::Replay(saved.reply)
            }
        },
    )
}

/// Read-only recovery follows the same keys without admitting an operation.
pub fn saved_request<R: serde::de::DeserializeOwned>(
    connection: &studio_runtime_store::Connection,
    key: RequestKey<'_>,
    signature: &str,
    now_ms: i64,
) -> Result<Option<R>, RequestResultError> {
    if let Some(saved) = replay_request(connection, key.raw, signature, now_ms)? {
        return Ok(Some(saved.reply));
    }
    let identity = request_identity(&key)?;
    Ok(replay_request(connection, identity.id(), signature, now_ms)?.map(|saved| saved.reply))
}

fn request_identity(key: &RequestKey<'_>) -> Result<RequestIdentity, RequestResultError> {
    RequestIdentity::new(key.namespace, key.authority, key.root, key.operation)
        .map_err(|_| RequestResultError::Expired)
}

/// Python `str.strip()` includes four ASCII separators Rust's whitespace predicate omits.
pub fn python_strip(value: &str) -> &str {
    value.trim_matches(|character: char| {
        character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
    })
}

#[derive(Debug, Error)]
pub enum EventWriteError {
    #[error(transparent)]
    RequestResult(#[from] RequestResultError),
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    InvalidResultSchema(String),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("Not implemented: requires cross-server/remote-events")]
    RemoteEventsUnavailable,
    #[error("Not implemented: requires worker-driver/work-assignment")]
    WorkerAssignmentUnavailable,
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

#[derive(Clone, Debug)]
pub struct AnnotationWrite {
    pub agent_id: String,
    pub id: Option<String>,
    pub path: String,
    pub line: i64,
    pub text: String,
    pub turn_id: Option<String>,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WriteResult {
    pub reference: RequestReference,
    pub event_inserted: bool,
}

#[derive(serde::Deserialize)]
struct LegacyAnnotationReply {
    id: String,
}

pub fn annotate(
    connection: &Transaction<'_>,
    input: AnnotationWrite,
) -> Result<WriteResult, EventWriteError> {
    let agent = checked_agent(connection, &input.agent_id)?;
    refuse_remote(&agent)?;
    let path = python_strip(&input.path).to_owned();
    let text = python_strip(&input.text).to_owned();
    if path.is_empty()
        || path.contains('\0')
        || text.is_empty()
        || !(0..=studio_runtime_schema::shared_values::MAX_SAFE_INTEGER).contains(&input.line)
    {
        return Err(EventWriteError::InvalidRequest("Invalid annotation"));
    }
    let turn_id = input.turn_id.as_deref().map(str::trim);
    if turn_id.is_some_and(str::is_empty) {
        return Err(EventWriteError::InvalidRequest("Invalid turn ID"));
    }
    let created = unix_time()?;
    let id = input
        .id
        .filter(|value| !value.is_empty())
        .unwrap_or_else(RequestIdentity::issue_root);
    let event_id = format!("annotation:{id}");
    let now_ms = (created * 1000.0) as i64;
    let authority = [input.agent_id.as_str()];
    let key = RequestKey::new("annotation", &authority, &id);
    if let Some(reference) =
        RequestResults::replay_reference(connection, key.raw, &input.signature, now_ms)?
    {
        return annotation_replay(reference, &id);
    }
    if let Some(saved) =
        replay_request::<LegacyAnnotationReply>(connection, key.raw, &input.signature, now_ms)?
    {
        return annotation_legacy_replay(saved.reply, &id);
    }
    let identity = request_identity(&key)?;
    if let Some(reference) =
        RequestResults::replay_reference(connection, identity.id(), &input.signature, now_ms)?
    {
        return annotation_replay(reference, &id);
    }
    let admitted =
        match begin_request::<LegacyAnnotationReply>(connection, key, &input.signature, now_ms)? {
            CallerAdmission::Replay(reply) => return annotation_legacy_replay(reply, &id),
            CallerAdmission::Fresh(admitted) => admitted,
        };
    if EventWriteRepository::event_exists(connection, &event_id)? {
        return Err(EventWriteError::Conflict(
            "The annotation event identity is already in use",
        ));
    }
    let record = AnnotationData {
        id: id.clone(),
        agent: input.agent_id.clone(),
        root_id: agent.root_id.0.clone(),
        path: path.clone(),
        line: input.line,
        text: text.clone(),
        created,
        turn_id: turn_id.map(str::to_owned),
    };
    let event_text = format!(
        "Review comment at {path}:{}{turn_text}\n{text}",
        input.line,
        turn_text = turn_id
            .map(|turn| format!(" (reported turn {turn})"))
            .unwrap_or_default(),
    );
    connection
        .execute(
            "INSERT INTO runtime_annotations(id,record) VALUES(?1,?2)",
            rusqlite::params![
                id,
                serde_json::to_string(&record).map_err(studio_runtime_store::StoreError::from)?
            ],
        )
        .map_err(studio_runtime_store::StoreError::from)?;
    let event_inserted = enqueue_event(connection, &agent, event_id, "user", event_text, created)?;
    let revision = connection
        .query_row(
            "SELECT rev FROM runtime_annotations WHERE id=?1",
            [&id],
            |row| row.get(0),
        )
        .map_err(studio_runtime_store::StoreError::from)?;
    let reference = RequestReference {
        kind: RequestReferenceKind::Annotation,
        id,
        revision,
    };
    let reference = RequestResults::settle_reference(connection, &admitted, &reference)?;
    Ok(WriteResult {
        reference,
        event_inserted,
    })
}

fn annotation_replay(
    reference: RequestReference,
    id: &str,
) -> Result<WriteResult, EventWriteError> {
    if reference.kind != RequestReferenceKind::Annotation || reference.id != id {
        return Err(invalid_stored("request_results", id));
    }
    Ok(WriteResult {
        reference,
        event_inserted: false,
    })
}

fn annotation_legacy_replay(
    reply: LegacyAnnotationReply,
    id: &str,
) -> Result<WriteResult, EventWriteError> {
    if reply.id != id {
        return Err(invalid_stored("request_results", id));
    }
    annotation_replay(
        RequestReference {
            kind: RequestReferenceKind::Annotation,
            id: reply.id,
            revision: 1,
        },
        id,
    )
}

pub(crate) fn refuse_remote(agent: &AgentRecord) -> Result<(), EventWriteError> {
    if truthy(agent.extra.get("remoteWorker")) || truthy(agent.extra.get("remoteAnchor")) {
        return Err(EventWriteError::RemoteEventsUnavailable);
    }
    Ok(())
}

pub fn enqueue_event(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    id: String,
    kind: &str,
    text: String,
    created: f64,
) -> Result<bool, EventWriteError> {
    enqueue(transaction, agent, id, kind, text, created, false)
}

pub fn enqueue_recovery_event(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    id: String,
    kind: &str,
    text: String,
    created: f64,
) -> Result<bool, EventWriteError> {
    enqueue(transaction, agent, id, kind, text, created, true)
}

fn enqueue(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    id: String,
    kind: &str,
    text: String,
    created: f64,
    recovery: bool,
) -> Result<bool, EventWriteError> {
    let mut metadata = QueueWriteRepository::typed_metadata(transaction, &id)?.unwrap_or_default();
    let delivery = metadata
        .delivery
        .value()
        .map(|v| v.moment())
        .map(Ok)
        .unwrap_or_else(|| DeliveryMoment::for_event(kind, &text, None))
        .map_err(|_| EventWriteError::InvalidRequest("Invalid delivery moment"))?;
    let existing = studio_runtime_store::QueueEventRepository::get(
        transaction,
        &QueueEventId::from(id.as_str()),
    )?
    .is_some();
    if !existing {
        metadata.delivery = studio_runtime_schema::queue_command::Field::Value(
            studio_runtime_schema::event_metadata::StoredDelivery::canonical(delivery),
        );
        QueueWriteRepository::put_typed_metadata(transaction, &id, &metadata)?;
    }
    let auto_wake = agent_auto_wake(agent)?;
    let recovery_pending = recovery && recovery_pending(agent);
    let event = QueueEvent {
        id: QueueEventId::from(id.as_str()),
        agent: AgentId::from(agent.id.0.as_str()),
        kind: kind.to_owned(),
        text,
        status: if auto_wake || recovery_pending {
            "pending"
        } else {
            "cancelled"
        }
        .to_owned(),
        created,
        epoch: agent.epoch,
        turn_id: None,
        error: None,
    };
    if let Some((server, link, is_anchor)) = remote_owner(agent)? {
        let request_id = match metadata.extensions.get("remoteRequestId") {
            Some(Value::String(root)) if !root.is_empty() => root.clone(),
            Some(_) => return Err(EventWriteError::Conflict("Remote event outcome is unknown")),
            None if existing => {
                return Err(EventWriteError::Conflict("Remote event outcome is unknown"));
            }
            None => {
                let root = uuid::Uuid::parse_str(&id)
                    .ok()
                    .filter(|id| id.get_version_num() == 7)
                    .map(|_| id.clone())
                    .unwrap_or_else(RequestIdentity::issue_root);
                metadata
                    .extensions
                    .insert("remoteRequestId".into(), json!(root));
                QueueWriteRepository::put_typed_metadata(transaction, &id, &metadata)?;
                root
            }
        };

        // Remote anchors forward user input through the paired input protocol;
        // remote workers use its worker form. Neither proxy queues native work
        // on this server.
        if !is_anchor {
            if !matches!(kind, "user" | "followup") {
                return Err(EventWriteError::RemoteEventsUnavailable);
            }
            let link_raw: Option<String> = transaction
                .query_row(
                    "SELECT record FROM runtime_server_links WHERE id=?1",
                    [&link],
                    |row| row.get(0),
                )
                .map_err(studio_runtime_store::StoreError::from)?;
            let link_record: Value = link_raw
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .ok_or(EventWriteError::Conflict("Remote event link is invalid"))?;
            if link_record.get("side").and_then(Value::as_str) != Some("remote")
                || link_record.get("server").and_then(Value::as_str) != Some(server.as_str())
                || !link_record
                    .get("workers")
                    .and_then(Value::as_array)
                    .is_some_and(|workers| {
                        workers
                            .iter()
                            .any(|worker| worker.as_str() == Some(agent.id.0.as_str()))
                    })
            {
                return Err(EventWriteError::Conflict("Remote event link is invalid"));
            }
            if metadata
                .assets
                .as_ref()
                .is_some_and(|assets| !assets.is_empty())
            {
                return Err(EventWriteError::InvalidRequest(
                    "Remote worker input does not support attachments",
                ));
            }
            let parent_epoch = link_record
                .get("parentEpoch")
                .and_then(Value::as_i64)
                .ok_or(EventWriteError::Conflict("Remote event link is invalid"))?;
            let remote_epoch = agent
                .extra
                .get("remoteEpoch")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let payload = json!({
                "link":link,
                "worker":agent.id.0,
                "kind":kind,
                "text":event.text,
                "delivery":delivery,
                "parentEpoch":parent_epoch,
                "epoch":remote_epoch,
                "controlEpoch":agent.epoch,
            });
            crate::multi_server_orchestration::enqueue_in_transaction(
                transaction,
                &server,
                "input",
                &payload,
                &request_id,
                created as i64,
            )
            .map_err(|_| EventWriteError::Conflict("Remote input could not be queued"))?;
            return Ok(true);
        }
        let link_raw: Option<String> = transaction
            .query_row(
                "SELECT record FROM runtime_server_links WHERE id=?1",
                [&link],
                |row| row.get(0),
            )
            .optional()
            .map_err(studio_runtime_store::StoreError::from)?;
        let link_record: Value = link_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .ok_or(EventWriteError::Conflict("Remote event link is invalid"))?;
        if link_record.get("side").and_then(Value::as_str) != Some("remote")
            || link_record.get("home").and_then(Value::as_str) != Some(server.as_str())
        {
            return Err(EventWriteError::Conflict("Remote event link is invalid"));
        }
        let parent_epoch = link_record
            .get("parentEpoch")
            .and_then(Value::as_i64)
            .ok_or(EventWriteError::Conflict("Remote event link is invalid"))?;
        if matches!(kind, "user" | "followup") {
            if is_anchor && kind != "user" {
                return Err(EventWriteError::RemoteEventsUnavailable);
            }
            if metadata
                .assets
                .as_ref()
                .is_some_and(|assets| !assets.is_empty())
            {
                return Err(EventWriteError::InvalidRequest(
                    "Remote anchor input does not support attachments",
                ));
            }
            let mut stored = event.clone();
            stored.status = "stored_only".to_owned();
            EventWriteRepository::insert_event(transaction, &stored)?;
            let mut payload = json!({
                "link": link,
                "parentEpoch": parent_epoch,
                "kind": event.kind,
                "text": event.text,
                "delivery": delivery,
            });
            payload[crate::multi_server_orchestration::REMOTE_ANCHOR_INPUT_FIELD] =
                Value::Bool(true);
            crate::multi_server_orchestration::enqueue_in_transaction(
                transaction,
                &server,
                "input",
                &payload,
                &request_id,
                created as i64,
            )
            .map_err(|_| EventWriteError::Conflict("Remote anchor input could not be queued"))?;
            return Ok(true);
        }
        let payload = json!({
            "link": link,
            "parentEpoch": parent_epoch,
            "kind": event.kind,
            "text": event.text,
            "delivery": delivery,
        });
        crate::multi_server_orchestration::enqueue_in_transaction(
            transaction,
            &server,
            "event",
            &payload,
            &request_id,
            created as i64,
        )
        .map_err(|_| EventWriteError::Conflict("Remote event could not be queued"))?;
        return Ok(true);
    }
    let inserted = EventWriteRepository::insert_event(transaction, &event)?;
    if inserted && !recovery_pending {
        if auto_wake {
            queue_agent(transaction, agent)?;
        }
        EventWriteRepository::mark_event_enqueued(transaction, &id, monotonic_ns())?;
    }
    Ok(inserted)
}

fn remote_owner(agent: &AgentRecord) -> Result<Option<(String, String, bool)>, EventWriteError> {
    let remote_worker = agent
        .extra
        .get("remoteWorker")
        .filter(|value| truthy(Some(value)));
    let remote_anchor = agent
        .extra
        .get("remoteAnchor")
        .filter(|value| truthy(Some(value)));
    let (value, is_anchor) = if let Some(value) = remote_anchor {
        (value, true)
    } else if let Some(value) = remote_worker {
        (value, false)
    } else {
        return Ok(None);
    };
    let server_field = if is_anchor { "home" } else { "server" };
    let server = value
        .get(server_field)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let link = value
        .get("link")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if server.is_empty() || link.is_empty() {
        return Err(EventWriteError::Conflict("Remote event owner is invalid"));
    }
    Ok(Some((server.to_owned(), link.to_owned(), is_anchor)))
}

pub fn checked_agent(
    connection: &Transaction<'_>,
    id: &str,
) -> Result<AgentRecord, EventWriteError> {
    let agent = AgentRepository::get(connection, &AgentId::from(id))?
        .ok_or(EventWriteError::InvalidRequest("Invalid request"))?;
    if agent.is_deleted() {
        return Err(EventWriteError::InvalidRequest("This agent was deleted"));
    }
    if agent.id.0 != id {
        return Err(invalid_stored("runtime_agents", id));
    }
    Ok(agent)
}

fn queue_agent(connection: &Transaction<'_>, agent: &AgentRecord) -> Result<(), EventWriteError> {
    let auto_wake = agent_auto_wake(agent)?;
    let status = match agent.status {
        AgentStatus::Running | AgentStatus::Starting => {
            agent.in_flight != Some(true)
                && agent.turn_id.as_deref().is_none_or(str::is_empty)
                && !truthy(agent.extra.get("startAttempt"))
        }
        AgentStatus::Approval => false,
        AgentStatus::Unknown
        | AgentStatus::Idle
        | AgentStatus::Queued
        | AgentStatus::Waiting
        | AgentStatus::WaitingOnMonitor
        | AgentStatus::Completed
        | AgentStatus::Interrupted
        | AgentStatus::Failed
        | AgentStatus::Blocked
        | AgentStatus::Paused
        | AgentStatus::Parked
        | AgentStatus::Abandoned
        | AgentStatus::CapacityRetry
        | AgentStatus::Other(_) => true,
    };
    if !auto_wake || truthy(agent.extra.get("nativeFailureHold")) || !status {
        return Ok(());
    }
    let mut updated = agent.clone();
    updated.status = AgentStatus::Queued;
    AgentRepository::put(connection, &updated)?;
    let projection =
        serde_json::to_value(&updated).map_err(studio_runtime_store::StoreError::from)?;
    crate::agents_create::publish_agent(connection, &projection)?;
    Ok(())
}

fn agent_auto_wake(agent: &AgentRecord) -> Result<bool, EventWriteError> {
    agent
        .auto_wake
        .ok_or_else(|| invalid_stored("runtime_agents", &agent.id.0))
}

pub fn unix_time() -> Result<f64, EventWriteError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| EventWriteError::Clock)
}

pub fn assert_delegation(agent: &AgentRecord) -> Result<(), EventWriteError> {
    let mode = agent.extra.get("agentMode").and_then(Value::as_str);
    let schema_version = agent
        .extra
        .get("subagentConcurrencyVersion")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let limit = if schema_version < 2 && mode == Some("single") {
        0
    } else {
        agent
            .extra
            .get("concurrency")
            .and_then(Value::as_i64)
            .unwrap_or(if mode == Some("single") { 0 } else { 32 })
    };
    if limit == 0 {
        return Err(EventWriteError::InvalidRequest(
            "Single agent mode disables new delegation. Complete the task in the lead chat or select Multi agent",
        ));
    }
    Ok(())
}

pub fn recovery_pending(agent: &AgentRecord) -> bool {
    let extra = &agent.extra;
    let matches = |recovery: &Value| {
        recovery.get("epoch").and_then(Value::as_i64) == Some(agent.epoch)
            && recovery.get("accountKey").and_then(Value::as_str)
                == Some(agent.account_key.as_str())
            && recovery.get("threadId").and_then(Value::as_str) == agent.thread_id.as_deref()
    };
    let root = extra
        .get("restartRecovery")
        .and_then(Value::as_object)
        .map(|recovery| {
            recovery.get("stage").and_then(Value::as_str) == Some("pending")
                && recovery.get("autoWake").and_then(Value::as_bool) == Some(true)
                && matches(&Value::Object(recovery.clone()))
        })
        .unwrap_or(false);
    let disconnect = extra
        .get("disconnectRecovery")
        .and_then(Value::as_object)
        .map(|recovery| {
            recovery.get("autoWake").and_then(Value::as_bool) == Some(true)
                && matches(&Value::Object(recovery.clone()))
        })
        .unwrap_or(false);
    let recovery = !agent_auto_wake(agent).unwrap_or(true)
        && agent.status != AgentStatus::Paused
        && !agent.is_deleted()
        && (root || disconnect);
    recovery || truthy(extra.get("moveImportPending"))
}

fn invalid_stored(object: &'static str, id: &str) -> EventWriteError {
    EventWriteError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

/// Preserve the pre-existing non-Unix implementation; local pipe timing is Unix-only.
#[cfg(not(unix))]
pub(crate) fn monotonic_ns() -> u128 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    time.tv_sec as u128 * 1_000_000_000 + time.tv_nsec as u128
}

#[cfg(test)]
mod tests {
    use super::{
        AnnotationWrite, EventWriteError, annotate, enqueue_event, enqueue_recovery_event,
    };
    use serde_json::{Value, json};

    #[test]
    fn pending_recovery_matches_typed_identity() {
        let mut agent = studio_runtime_store::AgentRecord::from_value(json!({"id":"a","rootId":"a","epoch":3,"status":"interrupted","accountKey":"account","threadId":"thread","autoWake":false,"restartRecovery":{"epoch":3,"accountKey":"account","threadId":"thread","stage":"pending","autoWake":true}})).unwrap();
        assert!(super::recovery_pending(&agent));
        agent.account_key = "other".into();
        assert!(!super::recovery_pending(&agent));
    }
    use std::collections::BTreeMap;
    use studio_runtime_schema::delivery::DeliveryMoment;
    use studio_runtime_store::request_results::RequestResultError;
    use studio_runtime_store::{
        AgentId, AgentRecord, AgentRepository, AgentStatus, Connection, QueueWriteRepository,
        Transaction,
    };

    #[test]
    fn retired_non_v7_request_is_expired_before_mutation() {
        let mut db = Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("receipts");
        db.execute_batch("CREATE TABLE counter(n INTEGER NOT NULL); INSERT INTO counter VALUES(0)")
            .expect("owner");
        let tx = db.transaction().expect("transaction");
        let root = "279bc183-b81e-4003-a05c-bb8e3f440bc4";
        let result = super::begin_request::<Value>(
            &tx,
            super::RequestKey::new("work", &[], root),
            "signature",
            1_000_001,
        );
        assert!(matches!(result, Err(RequestResultError::Expired)));
        assert_eq!(
            tx.query_row("SELECT n FROM counter", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                &studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_plans(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_annotations(id TEXT PRIMARY KEY,record TEXT NOT NULL,rev INTEGER NOT NULL DEFAULT 1);
                 CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_access_clients(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_server_links(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,server TEXT NOT NULL,body TEXT NOT NULL,state TEXT NOT NULL,result TEXT,attempts INTEGER NOT NULL DEFAULT 0,next_at REAL NOT NULL DEFAULT 0,error TEXT,created_at REAL NOT NULL DEFAULT 0,completed_at REAL NOT NULL DEFAULT 0,fingerprint TEXT,admission TEXT);
                 CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_generation(id INTEGER PRIMARY KEY,value INTEGER NOT NULL); INSERT INTO sync_generation VALUES(1,0);
                 INSERT INTO sync_entities VALUES('agent','a',1,'old','{\"collection\":\"agent\",\"id\":\"a\",\"value\":{\"id\":\"a\",\"status\":\"idle\"}}',0);"),
            )
            .expect("write schema creates");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        studio_runtime_store::receipt_schema::create(&connection).expect("request result schema");
        connection.execute_batch("DROP TABLE request_results; CREATE TABLE request_results(id TEXT PRIMARY KEY,signature TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'applied',ref_kind TEXT,ref_id TEXT,ref_rev INTEGER,result TEXT,created INTEGER NOT NULL); CREATE INDEX request_results_created ON request_results(created,id);").expect("reference receipt fixture");
        let agent = AgentRecord {
            account_key: "default".into(),
            auto_wake: Some(true),
            id: AgentId::from("a"),
            root_id: AgentId::from("a"),
            epoch: 3,
            status: AgentStatus::Idle,
            extra: BTreeMap::from([(
                "provider".to_owned(),
                json!(studio_runtime_schema::ProviderKind::Codex),
            )]),
            ..AgentRecord::test_defaults()
        };
        AgentRepository::put(&connection, &agent).expect("agent writes");
        connection
    }

    #[test]
    fn event_delivery_defaults_are_canonical_and_explicit_timing_wins() {
        let mut connection = database();
        with_transaction(&mut connection, |tx| {
            let agent = AgentRepository::get(tx, &AgentId::from("a"))?.expect("seeded agent");
            for (id, text, explicit, expected) in [
                (
                    "progress",
                    r#"{"importance":"progress"}"#,
                    None,
                    DeliveryMoment::NextTurn,
                ),
                (
                    "result",
                    r#"{"importance":"result"}"#,
                    None,
                    DeliveryMoment::CurrentTurn,
                ),
                (
                    "urgent-progress",
                    r#"{"importance":"progress"}"#,
                    Some("after_tool"),
                    DeliveryMoment::CurrentTurn,
                ),
                (
                    "later-result",
                    r#"{"importance":"result"}"#,
                    Some("next_turn"),
                    DeliveryMoment::NextTurn,
                ),
            ] {
                if let Some(moment) = explicit {
                    QueueWriteRepository::put_metadata(tx, id, &json!({"delivery":moment}))?;
                }
                assert!(enqueue_event(
                    tx,
                    &agent,
                    id.into(),
                    "agent_message",
                    text.into(),
                    1.0
                )?);
                let metadata = QueueWriteRepository::metadata(tx, id)?.expect("delivery persisted");
                assert_eq!(metadata["delivery"], json!(expected));
                assert!(!enqueue_event(
                    tx,
                    &agent,
                    id.into(),
                    "agent_message",
                    text.into(),
                    2.0
                )?);
                assert_eq!(QueueWriteRepository::metadata(tx, id)?, Some(metadata));
            }
            Ok(())
        })
        .expect("events admitted");
    }

    #[test]
    fn invalid_event_delivery_does_not_enqueue() {
        let mut connection = database();
        with_transaction(&mut connection, |tx| {
            let agent = AgentRepository::get(tx, &AgentId::from("a"))?.expect("seeded agent");
            // Simulate corrupt persisted metadata, bypassing the write validator.
            tx.execute(
                "INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2)",
                rusqlite::params![
                    "invalid-moment",
                    json!({"delivery":"later-ish"}).to_string()
                ],
            )
            .expect("corrupt metadata fixture");
            assert!(
                enqueue_event(
                    tx,
                    &agent,
                    "invalid-moment".into(),
                    "agent_message",
                    "{}".into(),
                    1.0
                )
                .is_err()
            );
            assert!(
                studio_runtime_store::QueueEventRepository::get(
                    tx,
                    &studio_runtime_store::QueueEventId::from("invalid-moment")
                )?
                .is_none()
            );
            Ok(())
        })
        .expect("invalid moment refused");
    }

    fn fixture_request_id(label: &str) -> String {
        static ISSUANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let root = uuid::Uuid::parse_str(
            ISSUANCE.get_or_init(studio_runtime_schema::persistence::RequestIdentity::issue_root),
        )
        .expect("v7");
        let mut bytes =
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, label.as_bytes()).into_bytes();
        bytes[..6].copy_from_slice(&root.as_bytes()[..6]);
        bytes[6] = (bytes[6] & 15) | 0x70;
        bytes[8] = (bytes[8] & 63) | 0x80;
        uuid::Uuid::from_bytes(bytes).to_string()
    }

    #[test]
    fn recovery_pending_event_is_stored_without_queue_or_timing_metadata() {
        let mut connection = database();
        let mut agent = AgentRepository::get(&connection, &AgentId::from("a"))
            .expect("agent reads")
            .expect("agent exists");
        agent
            .extra
            .insert("moveImportPending".to_owned(), json!(true));
        AgentRepository::put(&connection, &agent).expect("recovery state writes");
        let inserted = with_transaction(&mut connection, |transaction| {
            enqueue_recovery_event(
                transaction,
                &agent,
                "work-result:recovery".to_owned(),
                "work_review",
                "{}".to_owned(),
                1.0,
            )
        })
        .expect("recovery event stores");
        assert!(inserted);
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='work-result:recovery'",
                [],
                |row| row.get(0),
            )
            .expect("recovery event reads");
        assert_eq!(status, "pending");
        let metadata_count: i64 = connection
            .query_row("SELECT count(*) FROM runtime_event_meta", [], |row| {
                row.get(0)
            })
            .expect("event metadata count reads");
        assert_eq!(metadata_count, 1);
        let persisted_agent = AgentRepository::get(&connection, &AgentId::from("a"))
            .expect("agent reads")
            .expect("agent exists");
        assert_eq!(persisted_agent.status, AgentStatus::Idle);
    }

    #[test]
    fn annotation_replays_by_identity_and_refuses_conflicting_content() {
        let mut connection = database();
        let input = AnnotationWrite {
            agent_id: "a".into(),
            id: Some(fixture_request_id("note-1")),
            path: "file.rs".into(),
            line: 1,
            text: "comment".into(),
            turn_id: None,
            signature: "sig".into(),
        };
        let first = with_transaction(&mut connection, |transaction| {
            annotate(transaction, input.clone())
        })
        .expect("annotation writes");
        let before_replay = counts(&connection);
        let replay = with_transaction(&mut connection, |transaction| annotate(transaction, input))
            .expect("same annotation replays");
        assert_eq!(first.reference, replay.reference);
        assert!(!replay.event_inserted);
        assert_eq!(counts(&connection), before_replay);
        assert!(matches!(
            with_transaction(&mut connection, |transaction| annotate(
                transaction,
                AnnotationWrite {
                    text: " comment ".into(),
                    signature: "different-signature".into(),
                    ..annotation("note-1")
                }
            )),
            Err(EventWriteError::Conflict(_)
                | EventWriteError::RequestResult(RequestResultError::Conflict { .. }))
        ));
        assert_eq!(counts(&connection), before_replay);
        let conflict = AnnotationWrite {
            text: "changed".into(),
            signature: "other".into(),
            ..annotation("note-1")
        };
        assert!(matches!(
            with_transaction(&mut connection, |transaction| annotate(
                transaction,
                conflict
            )),
            Err(EventWriteError::Conflict(_)
                | EventWriteError::RequestResult(RequestResultError::Conflict { .. }))
        ));
        assert_eq!(counts(&connection), before_replay);
    }

    #[test]
    fn expired_annotation_never_creates_an_event() {
        let mut connection = database();
        let mut input = annotation("expired");
        input.id = Some(uuid::Uuid::from_u128((1u128 << 80) | (7 << 76) | (2 << 62)).to_string());
        let before = counts(&connection);
        assert!(matches!(
            with_transaction(&mut connection, |tx| annotate(tx, input)),
            Err(EventWriteError::RequestResult(
                RequestResultError::Identity(
                    studio_runtime_schema::persistence::RequestIdentityError::Expired { .. }
                )
            ))
        ));
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn imported_annotation_receipt_with_unverifiable_signature_is_a_conflict() {
        let mut connection = database();
        let saved = json!({
            "id":"note-1", "agent":"a", "rootId":"a", "path":"file.rs",
            "line":1, "text":"comment", "created":123.0
        });
        connection
            .execute(
                "INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)",
                (
                    "note-1",
                    "python-canonical-body-signature",
                    json!({"actor":"a","reply":saved,"references":[]}).to_string(),
                    chrono::Utc::now().timestamp_millis(),
                ),
            )
            .expect("Python-style receipt inserts");
        let before = counts(&connection);
        let replay = with_transaction(&mut connection, |transaction| {
            let mut input = annotation("note-1");
            input.id = Some("note-1".into());
            annotate(transaction, input)
        });
        assert!(matches!(
            replay,
            Err(EventWriteError::Conflict(_)
                | EventWriteError::RequestResult(RequestResultError::Conflict { .. }))
        ));
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn annotation_rejects_line_outside_safe_integer_range() {
        for line in [
            -1,
            studio_runtime_schema::shared_values::MAX_SAFE_INTEGER + 1,
        ] {
            let mut connection = database();
            let before = counts(&connection);
            let result = with_transaction(&mut connection, |tx| {
                annotate(
                    tx,
                    AnnotationWrite {
                        line,
                        ..annotation("bad-line")
                    },
                )
            });
            assert!(result.is_err());
            assert_eq!(counts(&connection), before);
        }
    }

    #[test]
    fn invalid_annotation_refuses_without_writes() {
        let mut connection = database();
        let before = counts(&connection);
        assert!(matches!(
            with_transaction(&mut connection, |transaction| {
                annotate(
                    transaction,
                    AnnotationWrite {
                        text: "  ".into(),
                        ..annotation("note-bad")
                    },
                )
            }),
            Err(EventWriteError::InvalidRequest(_))
        ));
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn active_event_rule_does_not_block_annotation_writes() {
        // Python enqueues user events through Runtime.enqueue,
        // whose rule hook matches only rule event names; user events are not one.
        let mut connection = database();
        let rule = r#"{"id":"r-event","agent":"a","status":"active","kind":"event","event":"worker_completed"}"#;
        connection
            .execute(
                "INSERT INTO runtime_rules(id,record) VALUES(?1,?2)",
                ["r-event", rule],
            )
            .expect("event rule inserts");
        let note = with_transaction(&mut connection, |transaction| {
            annotate(transaction, annotation("rule-note"))
        })
        .expect("an active event rule does not refuse an annotation");
        assert!(note.event_inserted);
        let stored: String = connection
            .query_row(
                "SELECT record FROM runtime_rules WHERE id='r-event'",
                [],
                |row| row.get(0),
            )
            .expect("rule reads");
        assert_eq!(stored, rule, "the rule is not triggered or changed");
    }

    #[test]
    fn invalid_typed_receipt_records_refuse_before_any_write() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    fixture_request_id("n"),
                    "sig",
                    json!({"actor":null,"reply":{},"references":[]}).to_string(),
                    chrono::Utc::now().timestamp_millis()
                ],
            )
            .expect("invalid imported reply");
        let before = counts(&connection);
        assert!(matches!(
            with_transaction(&mut connection, |tx| annotate(tx, annotation("n"))),
            Err(EventWriteError::RequestResult(RequestResultError::Json(_)))
        ));
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn failure_after_first_write_rolls_back_every_table() {
        let mut connection = database();
        connection.execute_batch("CREATE TRIGGER fail_event BEFORE INSERT ON runtime_events BEGIN SELECT RAISE(ABORT,'injected'); END;").expect("failure trigger installs");
        let before = counts(&connection);
        assert!(with_transaction(&mut connection, |tx| annotate(tx, annotation("n"))).is_err());
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn remote_agent_refuses_annotation_without_changes() {
        let mut connection = database();
        let mut agent = AgentRepository::get(&connection, &AgentId::from("a"))
            .expect("agent read")
            .expect("fixture");
        agent.extra.insert(
            "remoteWorker".into(),
            json!({"server":"remote","link":"remote-link"}),
        );
        AgentRepository::put(&connection, &agent).expect("remote fixture writes");
        let before = counts(&connection);
        assert!(matches!(
            with_transaction(&mut connection, |tx| annotate(tx, annotation("n"))),
            Err(EventWriteError::RemoteEventsUnavailable)
        ));
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn remote_worker_user_input_is_saved_as_stable_paired_intent() {
        let mut connection = database();
        let mut agent = AgentRepository::get(&connection, &AgentId::from("a"))
            .expect("agent read")
            .expect("fixture");
        agent.extra.insert(
            "remoteWorker".into(),
            json!({"server":"remote","link":"remote-link"}),
        );
        agent.extra.insert("remoteEpoch".into(), json!(3));
        AgentRepository::put(&connection, &agent).expect("remote fixture writes");
        connection.execute(
            "INSERT INTO runtime_server_links(id,record) VALUES(?1,?2)",
            ("remote-link", json!({"id":"remote-link","side":"remote","server":"remote","parentEpoch":4,"workers":["a"]}).to_string()),
        ).expect("remote link writes");
        connection.execute(
            "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
            ("remote", json!({"id":"remote","clientId":"client","kind":"server","label":"Remote","created":1,"publicKey":"public","status":"paired"}).to_string()),
        ).expect("paired peer writes");
        let unicode_text = "🙂".repeat(10_000);
        assert!(crate::coordination::message_text_within_limit(
            &unicode_text
        ));
        let result = with_transaction(&mut connection, |tx| {
            QueueWriteRepository::put_metadata(
                tx,
                "message-1",
                &json!({"assets":[],"delivery":"steer"}),
            )?;
            enqueue_event(
                tx,
                &agent,
                "message-1".into(),
                "user",
                unicode_text.clone(),
                100.0,
            )
        })
        .expect("paired input queues");
        assert!(result);
        let (server, body): (String, String) = connection
            .query_row("SELECT server,body FROM runtime_server_outbox", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("stable outbox row");
        let body: Value = serde_json::from_str(&body).expect("outbox envelope JSON");
        let root = body["requestId"].as_str().expect("root");
        assert_eq!(uuid::Uuid::parse_str(root).unwrap().get_version_num(), 7);
        assert_eq!(
            with_transaction(&mut connection, |tx| Ok(QueueWriteRepository::metadata(
                tx,
                "message-1"
            )?))
            .unwrap()
            .unwrap()["remoteRequestId"],
            root
        );
        with_transaction(&mut connection, |tx| {
            enqueue_event(
                tx,
                &agent,
                "message-1".into(),
                "user",
                unicode_text.clone(),
                101.0,
            )
        })
        .expect("lost response retry");
        let retry: String = connection
            .query_row("SELECT body FROM runtime_server_outbox", [], |r| r.get(0))
            .expect("same outbox");
        assert_eq!(
            serde_json::from_str::<Value>(&retry).unwrap()["requestId"],
            root
        );
        let count: i64 = connection
            .query_row("SELECT count(*) FROM runtime_server_outbox", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        let public = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        with_transaction(&mut connection, |tx| {
            enqueue_event(tx, &agent, public.clone(), "user", "public".into(), 102.0)
        })
        .expect("public root forwards");
        assert_eq!(
            with_transaction(&mut connection, |tx| Ok(QueueWriteRepository::metadata(
                tx, &public
            )?))
            .unwrap()
            .unwrap()["remoteRequestId"],
            public
        );
        for preserved in [
            json!(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, b"imported").to_string()),
            Value::Null,
        ] {
            with_transaction(&mut connection, |tx| {
                QueueWriteRepository::put_metadata(
                    tx,
                    "imported-event",
                    &json!({"remoteRequestId":preserved}),
                )?;
                Ok(())
            })
            .unwrap();
            let before = counts(&connection);
            assert!(
                with_transaction(&mut connection, |tx| enqueue_event(
                    tx,
                    &agent,
                    "imported-event".into(),
                    "user",
                    "old".into(),
                    103.0
                ))
                .is_err()
            );
            assert_eq!(
                counts(&connection),
                before,
                "an imported miss cannot mint or send"
            );
        }

        assert_eq!(server, "remote");
        assert_eq!(body["action"], "input");
        assert_eq!(body["payload"]["worker"], "a");
        assert_eq!(
            body["payload"]["delivery"],
            json!(DeliveryMoment::CurrentTurn)
        );
        assert_eq!(body["payload"]["parentEpoch"], 4);
        assert_eq!(body["payload"]["epoch"], 3);
        assert_eq!(body["payload"]["text"], unicode_text);
    }

    fn annotation(id: &str) -> AnnotationWrite {
        AnnotationWrite {
            agent_id: "a".into(),
            id: Some(fixture_request_id(id)),
            path: "file.rs".into(),
            line: 1,
            text: "comment".into(),
            turn_id: None,
            signature: "sig".into(),
        }
    }

    fn counts(connection: &Connection) -> std::collections::BTreeMap<String, Vec<Vec<String>>> {
        let tables = {
            let mut statement = connection
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .expect("table list prepares");
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("tables query")
                .collect::<Result<Vec<_>, _>>()
                .expect("table names")
        };
        tables
            .into_iter()
            .map(|table| {
                let mut statement = connection
                    .prepare(&format!("SELECT * FROM \"{table}\""))
                    .expect("snapshot prepares");
                let columns = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|index| row.get_ref(index).map(|value| format!("{value:?}")))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .expect("snapshot executes")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("snapshot reads");
                (table, rows)
            })
            .collect()
    }

    fn with_transaction<T>(
        connection: &mut Connection,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, EventWriteError>,
    ) -> Result<T, EventWriteError> {
        let transaction = connection.transaction().expect("transaction begins");
        match operation(&transaction) {
            Ok(value) => {
                transaction.commit().expect("transaction commits");
                Ok(value)
            }
            Err(error) => {
                drop(transaction);
                Err(error)
            }
        }
    }
}
