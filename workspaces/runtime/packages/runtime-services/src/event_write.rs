//! Atomic plan and annotation writes with durable scheduler events.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, EventWriteRepository, QueueEvent,
    QueueEventId, RulesRepository, Transaction,
};
use thiserror::Error;

static GENERATED_ANNOTATION_ID: AtomicU64 = AtomicU64::new(0);

/// Match Python's default JSON string escaping for durable operation signatures.
pub fn python_json(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::String(value) => python_quote(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!("{}: {}", python_quote(key), python_json(&values[key])))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

/// Python `str.strip()` includes four ASCII separators Rust's whitespace predicate omits.
pub fn python_strip(value: &str) -> &str {
    value.trim_matches(|character: char| {
        character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
    })
}

fn python_quote(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if c < ' ' || c == '\u{7f}' => {
                output.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if c as u32 > 0x7f => {
                let n = c as u32;
                if n <= 0xffff {
                    output.push_str(&format!("\\u{n:04x}"));
                } else {
                    let n = n - 0x10000;
                    output.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (n >> 10),
                        0xdc00 + (n & 0x3ff)
                    ));
                }
            }
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

#[derive(Debug, Error)]
pub enum EventWriteError {
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("Not implemented: requires rule-driver/event-rules")]
    RuleDriverUnavailable,
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
pub struct PlanWrite {
    pub agent_id: String,
    pub request_id: Option<String>,
    pub expected_version: i64,
    pub text: String,
    pub signature: String,
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
    pub record: Value,
    pub event_inserted: bool,
}

pub fn save_plan(
    connection: &Transaction<'_>,
    input: PlanWrite,
) -> Result<WriteResult, EventWriteError> {
    save_plan_validated(connection, input, |_| true)
}

pub fn save_plan_validated(
    connection: &Transaction<'_>,
    input: PlanWrite,
    validate: impl Fn(&Value) -> bool,
) -> Result<WriteResult, EventWriteError> {
    let agent = checked_agent(connection, &input.agent_id)?;
    refuse_remote(&agent)?;
    let text = python_strip(&input.text).to_owned();
    let requested_version = input.expected_version;
    let version = requested_version + 1;
    let event_id = format!("plan:{}:{version}", input.agent_id);
    let receipt_id = input
        .request_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("plan-request:{value}"))
        .unwrap_or_else(|| event_id.clone());
    if let Some(receipt) = EventWriteRepository::receipt(connection, &receipt_id)? {
        if receipt.signature != input.signature {
            return Err(EventWriteError::Conflict(
                "This request id has different content",
            ));
        }
        let saved: Value = serde_json::from_str(&receipt.result)
            .map_err(|_| invalid_stored("runtime_operation_receipts", &receipt_id))?;
        if !validate(&saved) {
            return Err(invalid_stored("runtime_plans", &input.agent_id));
        }
        return Ok(WriteResult {
            record: saved,
            event_inserted: false,
        });
    }
    if EventWriteRepository::event_exists(connection, &event_id)? {
        return Err(EventWriteError::Conflict(
            "The plan event identity is already in use",
        ));
    }
    refuse_if_event_rule_applies(connection, &input.agent_id)?;

    let mut plan = match EventWriteRepository::plan(connection, &input.agent_id)? {
        Some(record) => {
            let value: Value = serde_json::from_str(&record)
                .map_err(|_| invalid_stored("runtime_plans", &input.agent_id))?;
            if value.get("id").and_then(Value::as_str) != Some(input.agent_id.as_str()) {
                return Err(invalid_stored("runtime_plans", &input.agent_id));
            }
            let actual = value
                .get("version")
                .and_then(Value::as_i64)
                .filter(|value| *value >= 0)
                .ok_or_else(|| invalid_stored("runtime_plans", &input.agent_id))?;
            if actual != requested_version {
                return Err(EventWriteError::Conflict(
                    "The plan changed. Reload before saving",
                ));
            }
            value
        }
        None if requested_version == 0 => json!({
            "id": input.agent_id.clone(),
            "rootId": agent.root_id.0,
            "text": "",
            "version": 0,
            "updated": null,
            "steps": []
        }),
        None => {
            return Err(EventWriteError::Conflict(
                "The plan changed. Reload before saving",
            ));
        }
    };
    let plan_object = plan
        .as_object_mut()
        .ok_or_else(|| invalid_stored("runtime_plans", &input.agent_id))?;
    plan_object.insert("text".to_owned(), Value::String(text.clone()));
    plan_object.insert("version".to_owned(), json!(version));
    plan_object.insert("updated".to_owned(), json!(unix_time()?));
    let record = plan;
    if !validate(&record) {
        return Err(invalid_stored("runtime_plans", &input.agent_id));
    }
    let created = unix_time()?;
    let event_text = format!("The user updated the shared plan:\n{text}");
    let record_json = serialize(&record)?;
    EventWriteRepository::put_plan(connection, &input.agent_id, &record_json)?;
    let event_inserted = enqueue_event(
        connection,
        &agent,
        event_id,
        "plan_update",
        event_text,
        created,
    )?;
    EventWriteRepository::put_receipt(connection, &receipt_id, &input.signature, &record_json)?;
    Ok(WriteResult {
        record,
        event_inserted,
    })
}

pub fn annotate(
    connection: &Transaction<'_>,
    input: AnnotationWrite,
) -> Result<WriteResult, EventWriteError> {
    annotate_validated(connection, input, |_| true)
}

pub fn annotate_validated(
    connection: &Transaction<'_>,
    input: AnnotationWrite,
    validate: impl Fn(&Value) -> bool,
) -> Result<WriteResult, EventWriteError> {
    let agent = checked_agent(connection, &input.agent_id)?;
    refuse_remote(&agent)?;
    let path = python_strip(&input.path).to_owned();
    let text = python_strip(&input.text).to_owned();
    if path.is_empty() || path.contains('\0') || text.is_empty() {
        return Err(EventWriteError::InvalidRequest("Invalid annotation"));
    }
    let turn_id = input.turn_id.as_deref().map(str::trim);
    if turn_id.is_some_and(str::is_empty) {
        return Err(EventWriteError::InvalidRequest("Invalid turn ID"));
    }
    let created = unix_time()?;
    let has_request_identity = input.id.as_deref().is_some_and(|value| !value.is_empty());
    let id = input
        .id
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| generated_annotation_id(created));
    let event_id = format!("annotation:{id}");
    if let Some(receipt) = EventWriteRepository::receipt(connection, &id)? {
        let record: Value = serde_json::from_str(&receipt.result)
            .map_err(|_| invalid_stored("runtime_operation_receipts", &id))?;
        if receipt.signature != input.signature {
            return Err(EventWriteError::Conflict(
                "This request id has different content",
            ));
        }
        if !validate(&record) {
            return Err(invalid_stored("runtime_annotations", &id));
        }
        return Ok(WriteResult {
            record,
            event_inserted: false,
        });
    }
    if EventWriteRepository::event_exists(connection, &event_id)? {
        return Err(EventWriteError::Conflict(
            "The annotation event identity is already in use",
        ));
    }
    refuse_if_event_rule_applies(connection, &input.agent_id)?;

    let mut record = json!({
        "id": id,
        "agent": input.agent_id.clone(),
        "rootId": agent.root_id.0,
        "path": path,
        "line": input.line,
        "text": text,
        "created": created,
    });
    if let Some(turn_id) = turn_id {
        record["turnId"] = Value::String(turn_id.to_owned());
    }
    if !validate(&record) {
        return Err(invalid_stored("runtime_annotations", &id));
    }
    let event_text = format!(
        "Review comment at {path}:{}{turn_text}\n{text}",
        input.line,
        turn_text = turn_id
            .map(|turn| format!(" (reported turn {turn})"))
            .unwrap_or_default(),
    );
    let record_json = serialize(&record)?;
    EventWriteRepository::put_annotation(connection, &id, &record_json)?;
    let event_inserted = enqueue_event(connection, &agent, event_id, "user", event_text, created)?;
    if has_request_identity {
        EventWriteRepository::put_receipt(connection, &id, &input.signature, &record_json)?;
    }
    Ok(WriteResult {
        record,
        event_inserted,
    })
}

fn refuse_if_event_rule_applies(
    connection: &Transaction<'_>,
    agent_id: &str,
) -> Result<(), EventWriteError> {
    let (rules, _) = RulesRepository::list(connection)?;
    for rule in rules {
        let record: Value = serde_json::from_str(&rule.record)
            .map_err(|_| invalid_stored("runtime_rules", &rule.id))?;
        if record.get("agent").and_then(Value::as_str) == Some(agent_id)
            && record.get("kind").and_then(Value::as_str) == Some("event")
            && record.get("status").and_then(Value::as_str) == Some("active")
        {
            return Err(EventWriteError::RuleDriverUnavailable);
        }
    }
    Ok(())
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
    let auto_wake = agent_auto_wake(agent)?;
    let event = QueueEvent {
        id: QueueEventId::from(id.as_str()),
        agent: AgentId::from(agent.id.0.as_str()),
        kind: kind.to_owned(),
        text,
        status: if auto_wake || recovery && recovery_pending(agent) {
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
    let inserted = EventWriteRepository::insert_event(transaction, &event)?;
    if inserted {
        if auto_wake {
            queue_agent(transaction, agent)?;
        }
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        EventWriteRepository::mark_event_enqueued(
            transaction,
            &id,
            START.get_or_init(Instant::now).elapsed().as_nanos(),
        )?;
    }
    Ok(inserted)
}

pub fn checked_agent(
    connection: &Transaction<'_>,
    id: &str,
) -> Result<AgentRecord, EventWriteError> {
    let agent = AgentRepository::get(connection, &AgentId::from(id))?
        .ok_or(EventWriteError::InvalidRequest("Invalid request"))?;
    if truthy(agent.extra.get("deletedAt")) {
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
            !truthy(agent.extra.get("inFlight"))
                && !truthy(agent.extra.get("turnId"))
                && !truthy(agent.extra.get("startAttempt"))
        }
        AgentStatus::Approval => false,
        _ => true,
    };
    if !auto_wake || truthy(agent.extra.get("nativeFailureHold")) || !status {
        return Ok(());
    }
    let mut updated = agent.clone();
    updated.status = AgentStatus::Queued;
    AgentRepository::put(connection, &updated)?;
    EventWriteRepository::set_agent_sync_status(connection, &updated.id.0, "queued")?;
    Ok(())
}

fn agent_auto_wake(agent: &AgentRecord) -> Result<bool, EventWriteError> {
    agent
        .extra
        .get("autoWake")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid_stored("runtime_agents", &agent.id.0))
}

pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
        Some(Value::Bool(true)) => true,
    }
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

pub fn read_receipt(
    transaction: &Transaction<'_>,
    request_id: Option<&str>,
    signature: &str,
    object: &'static str,
    identity: &str,
    validate: impl Fn(&Value) -> bool,
) -> Result<Option<Value>, EventWriteError> {
    let Some(request_id) = request_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let Some(receipt) = EventWriteRepository::receipt(transaction, request_id)? else {
        return Ok(None);
    };
    if receipt.signature != signature {
        return Err(EventWriteError::Conflict(
            "This request id has different content",
        ));
    }
    let result: Value = serde_json::from_str(&receipt.result)
        .map_err(|_| invalid_stored("runtime_operation_receipts", request_id))?;
    if !validate(&result) {
        return Err(invalid_stored(object, identity));
    }
    Ok(Some(result))
}

pub fn save_receipt(
    transaction: &Transaction<'_>,
    request_id: Option<&str>,
    signature: &str,
    result: &Value,
) -> Result<(), EventWriteError> {
    if let Some(request_id) = request_id.filter(|id| !id.is_empty()) {
        EventWriteRepository::put_receipt(transaction, request_id, signature, &serialize(result)?)?;
    }
    Ok(())
}

fn recovery_pending(agent: &AgentRecord) -> bool {
    let extra = &agent.extra;
    let matches = |recovery: &Value| {
        ["epoch", "accountKey", "threadId"]
            .iter()
            .all(|key| recovery.get(*key) == extra.get(*key))
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
        && !truthy(extra.get("deletedAt"))
        && (root || disconnect);
    recovery || truthy(extra.get("moveImportPending"))
}

fn serialize(value: &Value) -> Result<String, EventWriteError> {
    serde_json::to_string(value)
        .map_err(studio_runtime_store::StoreError::from)
        .map_err(EventWriteError::from)
}

fn generated_annotation_id(created: f64) -> String {
    let sequence = GENERATED_ANNOTATION_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "annotation-{}-{sequence}",
        (created * 1_000_000_000.0) as u128
    )
}

fn invalid_stored(object: &'static str, id: &str) -> EventWriteError {
    EventWriteError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{AnnotationWrite, EventWriteError, PlanWrite, annotate, save_plan};
    use serde_json::json;
    use std::collections::BTreeMap;
    use studio_runtime_store::{
        AgentId, AgentRecord, AgentRepository, AgentStatus, Connection, Transaction,
    };

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_plans(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_annotations(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
                 CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);
                 INSERT INTO sync_entities VALUES('agent','a',1,'old','{\"collection\":\"agent\",\"id\":\"a\",\"value\":{\"id\":\"a\",\"status\":\"idle\"}}',0);",
            )
            .expect("write schema creates");
        let agent = AgentRecord {
            id: AgentId::from("a"),
            root_id: AgentId::from("a"),
            epoch: 3,
            status: AgentStatus::Idle,
            extra: BTreeMap::from([
                ("autoWake".to_owned(), json!(true)),
                ("deletedAt".to_owned(), serde_json::Value::Null),
            ]),
        };
        AgentRepository::put(&connection, &agent).expect("agent writes");
        connection
    }

    fn plan_input(version: i64, text: &str, signature: &str) -> PlanWrite {
        PlanWrite {
            agent_id: "a".into(),
            request_id: None,
            expected_version: version,
            text: text.into(),
            signature: signature.into(),
        }
    }

    #[test]
    fn plan_write_replays_identically_and_refuses_conflicting_replay() {
        let mut connection = database();
        let first = with_transaction(&mut connection, |transaction| {
            save_plan(transaction, plan_input(0, "hello", "one"))
        })
        .expect("plan saves");
        let before_replay = counts(&connection);
        let replay = with_transaction(&mut connection, |transaction| {
            save_plan(transaction, plan_input(0, "hello", "one"))
        })
        .expect("same plan identity replays");
        assert_eq!(first.record, replay.record);
        assert!(!replay.event_inserted);
        assert_eq!(counts(&connection), before_replay);
        assert!(matches!(
            with_transaction(&mut connection, |transaction| {
                save_plan(transaction, plan_input(0, "different", "two"))
            }),
            Err(EventWriteError::Conflict(_))
        ));
        assert_eq!(counts(&connection), before_replay);
    }

    #[test]
    fn plan_version_refusal_leaves_every_table_unchanged() {
        let mut connection = database();
        let before = counts(&connection);
        assert!(matches!(
            with_transaction(&mut connection, |transaction| {
                save_plan(transaction, plan_input(2, "wrong version", "x"))
            }),
            Err(EventWriteError::Conflict(_))
        ));
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn annotation_replays_by_identity_and_refuses_conflicting_content() {
        let mut connection = database();
        let input = AnnotationWrite {
            agent_id: "a".into(),
            id: Some("note-1".into()),
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
        assert_eq!(first.record, replay.record);
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
            Err(EventWriteError::Conflict(_))
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
            Err(EventWriteError::Conflict(_))
        ));
        assert_eq!(counts(&connection), before_replay);
    }

    #[test]
    fn python_annotation_receipt_with_unverifiable_signature_is_a_conflict() {
        let mut connection = database();
        let saved = json!({
            "id":"note-1", "agent":"a", "rootId":"a", "path":"file.rs",
            "line":1, "text":"comment", "created":123.0
        });
        let saved_json = serde_json::to_string(&saved).expect("saved annotation serializes");
        connection
            .execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3)",
                ("note-1", "python-canonical-body-signature", saved_json),
            )
            .expect("Python-style receipt inserts");
        let before = counts(&connection);
        let replay = with_transaction(&mut connection, |transaction| {
            annotate(transaction, annotation("note-1"))
        });
        assert!(matches!(replay, Err(EventWriteError::Conflict(_))));
        assert_eq!(counts(&connection), before);
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
    fn no_active_event_rule_allows_writes() {
        let mut connection = database();
        let result = with_transaction(&mut connection, |transaction| {
            save_plan(transaction, plan_input(0, "allowed", "sig"))
        });
        assert!(result.is_ok());
    }

    #[test]
    fn active_event_rule_refuses_before_any_write() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO runtime_rules(id,record) VALUES(?1,?2)",
                [
                    "r-event",
                    r#"{"id":"r-event","agent":"a","status":"active","kind":"event"}"#,
                ],
            )
            .expect("event rule inserts");
        let before = counts(&connection);
        assert!(matches!(
            with_transaction(&mut connection, |transaction| {
                save_plan(transaction, plan_input(0, "blocked", "sig"))
            }),
            Err(EventWriteError::RuleDriverUnavailable)
        ));
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn stored_validation_refuses_before_any_write() {
        let mut connection = database();
        let before = counts(&connection);
        assert!(matches!(
            with_transaction(&mut connection, |tx| super::save_plan_validated(
                tx,
                plan_input(0, "x", "s"),
                |_| false
            )),
            Err(EventWriteError::InvalidStoredObject { .. })
        ));
        assert!(matches!(
            with_transaction(&mut connection, |tx| super::annotate_validated(
                tx,
                annotation("n"),
                |_| false
            )),
            Err(EventWriteError::InvalidStoredObject { .. })
        ));
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn failure_after_first_write_rolls_back_every_table() {
        let mut connection = database();
        connection.execute_batch("CREATE TRIGGER fail_event BEFORE INSERT ON runtime_events BEGIN SELECT RAISE(ABORT,'injected'); END;").expect("failure trigger installs");
        let before = counts(&connection);
        assert!(
            with_transaction(&mut connection, |tx| save_plan(tx, plan_input(0, "x", "s"))).is_err()
        );
        assert_eq!(counts(&connection), before);
        assert!(with_transaction(&mut connection, |tx| annotate(tx, annotation("n"))).is_err());
        assert_eq!(counts(&connection), before);
    }

    #[test]
    fn remote_agent_refuses_both_writes_without_changes() {
        let mut connection = database();
        let mut agent = AgentRepository::get(&connection, &AgentId::from("a"))
            .expect("agent read")
            .expect("fixture");
        agent
            .extra
            .insert("remoteWorker".into(), json!({"server":"remote"}));
        AgentRepository::put(&connection, &agent).expect("remote fixture writes");
        let before = counts(&connection);
        assert!(matches!(
            with_transaction(&mut connection, |tx| save_plan(tx, plan_input(0, "x", "s"))),
            Err(EventWriteError::RemoteEventsUnavailable)
        ));
        assert!(matches!(
            with_transaction(&mut connection, |tx| annotate(tx, annotation("n"))),
            Err(EventWriteError::RemoteEventsUnavailable)
        ));
        assert_eq!(counts(&connection), before);
    }

    fn annotation(id: &str) -> AnnotationWrite {
        AnnotationWrite {
            agent_id: "a".into(),
            id: Some(id.into()),
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
