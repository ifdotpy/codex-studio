//! Atomic plan and annotation writes with durable scheduler events.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, EventWriteRepository, QueueEvent,
    QueueEventId, RulesRepository, Transaction,
};
use thiserror::Error;

static GENERATED_ANNOTATION_ID: AtomicU64 = AtomicU64::new(0);
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Error)]
pub enum EventWriteError {
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("Not implemented: requires rule-driver/event-rules")]
    RuleDriverUnavailable,
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
    let agent = checked_agent(connection, &input.agent_id)?;
    let text = text_field(&input.text, "a plan", 64_000, true)?;
    let requested_version = input.expected_version;
    if !(0..MAX_SAFE_INTEGER).contains(&requested_version) {
        return Err(EventWriteError::InvalidRequest("Invalid request"));
    }
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
        return Ok(WriteResult {
            record: serde_json::from_str(&receipt.result)
                .map_err(|_| invalid_stored("runtime_operation_receipts", &receipt_id))?,
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
    let created = unix_time()?;
    let event_text = format!("The user updated the shared plan:\n{text}");
    let event_status = if agent_auto_wake(&agent)? {
        "pending"
    } else {
        "cancelled"
    };

    EventWriteRepository::put_plan(connection, &input.agent_id, &serialize(&record)?)?;
    let event_inserted = EventWriteRepository::insert_event(
        connection,
        &QueueEvent {
            id: QueueEventId::from(event_id),
            agent: AgentId::from(input.agent_id.as_str()),
            kind: "plan_update".to_owned(),
            text: event_text,
            status: event_status.to_owned(),
            created,
            epoch: agent.epoch,
            turn_id: None,
            error: None,
        },
    )?;
    if event_inserted {
        queue_agent(connection, &agent)?;
    }
    EventWriteRepository::put_receipt(
        connection,
        &receipt_id,
        &input.signature,
        &serialize(&record)?,
    )?;
    Ok(WriteResult {
        record,
        event_inserted,
    })
}

pub fn annotate(
    connection: &Transaction<'_>,
    input: AnnotationWrite,
) -> Result<WriteResult, EventWriteError> {
    let agent = checked_agent(connection, &input.agent_id)?;
    let path = text_field(&input.path, "a file path", 4_096, false)?;
    let text = text_field(&input.text, "a comment", 32_000, false)?;
    if !(1..=MAX_SAFE_INTEGER).contains(&input.line) || path.contains('\0') {
        return Err(EventWriteError::InvalidRequest(
            "Supply a positive line number",
        ));
    }
    let turn_id = input.turn_id.as_deref().map(str::trim);
    if turn_id.is_some_and(|value| value.is_empty() || value.chars().count() > 200) {
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
        if receipt.signature != input.signature
            && !annotation_matches_request(
                &record,
                &id,
                &input.agent_id,
                &path,
                input.line,
                &text,
                turn_id,
            )
        {
            return Err(EventWriteError::Conflict(
                "This request id has different content",
            ));
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
    let event_text = format!(
        "Review comment at {}:{}{}\n{}",
        record["path"].as_str().unwrap_or_default(),
        input.line,
        record
            .get("turnId")
            .and_then(Value::as_str)
            .map(|turn_id| format!(" (reported turn {turn_id})"))
            .unwrap_or_default(),
        record["text"].as_str().unwrap_or_default(),
    );
    let event_status = if agent_auto_wake(&agent)? {
        "pending"
    } else {
        "cancelled"
    };

    EventWriteRepository::put_annotation(connection, &id, &serialize(&record)?)?;
    let event_inserted = EventWriteRepository::insert_event(
        connection,
        &QueueEvent {
            id: QueueEventId::from(event_id),
            agent: AgentId::from(input.agent_id.as_str()),
            kind: "user".to_owned(),
            text: event_text,
            status: event_status.to_owned(),
            created,
            epoch: agent.epoch,
            turn_id: None,
            error: None,
        },
    )?;
    if event_inserted {
        queue_agent(connection, &agent)?;
    }
    if has_request_identity {
        EventWriteRepository::put_receipt(connection, &id, &input.signature, &serialize(&record)?)?;
    }
    Ok(WriteResult {
        record,
        event_inserted,
    })
}

fn annotation_matches_request(
    record: &Value,
    id: &str,
    agent_id: &str,
    path: &str,
    line: i64,
    text: &str,
    turn_id: Option<&str>,
) -> bool {
    record.get("id").and_then(Value::as_str) == Some(id)
        && record.get("agent").and_then(Value::as_str) == Some(agent_id)
        && record.get("path").and_then(Value::as_str) == Some(path)
        && record.get("line").and_then(Value::as_i64) == Some(line)
        && record.get("text").and_then(Value::as_str) == Some(text)
        && record.get("turnId").and_then(Value::as_str) == turn_id
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

fn checked_agent(connection: &Transaction<'_>, id: &str) -> Result<AgentRecord, EventWriteError> {
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

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

fn text_field(
    value: &str,
    name: &'static str,
    maximum: usize,
    allow_empty: bool,
) -> Result<String, EventWriteError> {
    let value = value.trim();
    if value.chars().count() > maximum || (!allow_empty && value.is_empty()) {
        return Err(EventWriteError::InvalidRequest(match name {
            "a plan" => "Supply a plan with 0 to 64000 characters",
            "a file path" => "Supply a file path with 1 to 4096 characters",
            _ => "Supply a comment with 1 to 32000 characters",
        }));
    }
    Ok(value.to_owned())
}

fn unix_time() -> Result<f64, EventWriteError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| EventWriteError::Clock)
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
        let replay = with_transaction(&mut connection, |transaction| {
            save_plan(transaction, plan_input(0, "hello", "one"))
        })
        .expect("same plan identity replays");
        assert_eq!(first.record, replay.record);
        assert!(!replay.event_inserted);
        assert!(matches!(
            with_transaction(&mut connection, |transaction| {
                save_plan(transaction, plan_input(0, "different", "two"))
            }),
            Err(EventWriteError::Conflict(_))
        ));
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM runtime_events", [], |row| row
                    .get::<_, i64>(0))
                .expect("event count reads"),
            1
        );
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
        let replay = with_transaction(&mut connection, |transaction| annotate(transaction, input))
            .expect("same annotation replays");
        assert_eq!(first.record, replay.record);
        assert!(!replay.event_inserted);
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
        assert_eq!(counts(&connection), (0, 1, 1));
    }

    #[test]
    fn python_style_annotation_receipt_replays_by_saved_content() {
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
        })
        .expect("same saved annotation content replays across runtimes");
        assert_eq!(replay.record, saved);
        assert!(!replay.event_inserted);
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
                        line: 0,
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

    fn counts(connection: &Connection) -> (i64, i64, i64) {
        (
            connection
                .query_row("SELECT COUNT(*) FROM runtime_plans", [], |row| row.get(0))
                .expect("plan count reads"),
            connection
                .query_row("SELECT COUNT(*) FROM runtime_annotations", [], |row| {
                    row.get(0)
                })
                .expect("annotation count reads"),
            connection
                .query_row("SELECT COUNT(*) FROM runtime_events", [], |row| row.get(0))
                .expect("event count reads"),
        )
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
