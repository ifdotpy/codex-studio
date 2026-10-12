use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::StoreResult;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(AgentId);
string_id!(TaskId);
string_id!(QueueEventId);
string_id!(ChatMessageId);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentStatus {
    Unknown,
    Idle,
    Queued,
    Starting,
    Running,
    Waiting,
    Approval,
    Completed,
    Interrupted,
    Failed,
    Blocked,
    Paused,
    Parked,
    Abandoned,
    CapacityRetry,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub id: AgentId,
    pub root_id: AgentId,
    pub epoch: i64,
    pub status: AgentStatus,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Ready,
    Running,
    Blocked,
    Review,
    Accepted,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: TaskId,
    pub root_id: AgentId,
    pub status: WorkStatus,
    pub title: String,
    pub description: String,
    pub owner: Option<AgentId>,
    pub created: f64,
    pub updated: f64,
    pub created_by: AgentId,
    pub version: i64,
    pub dependencies: Vec<TaskId>,
    pub decisions: Vec<Value>,
    pub results: Vec<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueEvent {
    pub id: QueueEventId,
    pub agent: AgentId,
    pub kind: String,
    pub text: String,
    pub status: String,
    pub created: f64,
    pub epoch: i64,
    pub turn_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub sequence: i64,
    pub id: ChatMessageId,
    pub room: String,
    pub sender: String,
    pub text: String,
    pub created: f64,
    pub deliveries: Value,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AgentRepository;

impl AgentRepository {
    pub fn get(connection: &Connection, id: &AgentId) -> StoreResult<Option<AgentRecord>> {
        get_json_record(connection, "runtime_agents", &id.0)
    }

    pub fn put(connection: &Connection, record: &AgentRecord) -> StoreResult<()> {
        put_json_record(connection, "runtime_agents", &record.id.0, record)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TaskRepository;

impl TaskRepository {
    pub fn get(connection: &Connection, id: &TaskId) -> StoreResult<Option<TaskRecord>> {
        get_json_record(connection, "runtime_tasks", &id.0)
    }

    pub fn put(connection: &Connection, record: &TaskRecord) -> StoreResult<()> {
        put_json_record(connection, "runtime_tasks", &record.id.0, record)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QueueEventRepository;

impl QueueEventRepository {
    pub fn get(connection: &Connection, id: &QueueEventId) -> StoreResult<Option<QueueEvent>> {
        let event = connection
            .query_row(
                "SELECT id, agent, kind, text, status, created, epoch, turn_id, error \
                 FROM runtime_events WHERE id = ?1",
                [&id.0],
                |row| {
                    Ok(QueueEvent {
                        id: QueueEventId(row.get(0)?),
                        agent: AgentId(row.get(1)?),
                        kind: row.get(2)?,
                        text: row.get(3)?,
                        status: row.get(4)?,
                        created: row.get(5)?,
                        epoch: row.get(6)?,
                        turn_id: row.get(7)?,
                        error: row.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(event)
    }

    pub fn put(connection: &Connection, event: &QueueEvent) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_events(id, agent, kind, text, status, created, epoch, turn_id, error) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET agent=excluded.agent, kind=excluded.kind, \
             text=excluded.text, status=excluded.status, created=excluded.created, \
             epoch=excluded.epoch, turn_id=excluded.turn_id, error=excluded.error",
            params![
                event.id.0,
                event.agent.0,
                event.kind,
                event.text,
                event.status,
                event.created,
                event.epoch,
                event.turn_id,
                event.error,
            ],
        )?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MessageRepository;

impl MessageRepository {
    pub fn get(connection: &Connection, id: &ChatMessageId) -> StoreResult<Option<ChatMessage>> {
        let message = connection
            .query_row(
                "SELECT seq, id, room, sender, text, created, deliveries \
                 FROM runtime_chat_messages WHERE id = ?1",
                [&id.0],
                |row| {
                    let deliveries: String = row.get(6)?;
                    let parsed = serde_json::from_str(&deliveries).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    Ok(ChatMessage {
                        sequence: row.get(0)?,
                        id: ChatMessageId(row.get(1)?),
                        room: row.get(2)?,
                        sender: row.get(3)?,
                        text: row.get(4)?,
                        created: row.get(5)?,
                        deliveries: parsed,
                    })
                },
            )
            .optional()?;
        Ok(message)
    }

    pub fn put(connection: &Connection, message: &ChatMessage) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_chat_messages(id, room, sender, text, created, deliveries) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(id) DO UPDATE SET room=excluded.room, sender=excluded.sender, \
             text=excluded.text, created=excluded.created, deliveries=excluded.deliveries",
            params![
                message.id.0,
                message.room,
                message.sender,
                message.text,
                message.created,
                serde_json::to_string(&message.deliveries)?,
            ],
        )?;
        Ok(())
    }
}

fn get_json_record<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    table: &str,
    id: &str,
) -> StoreResult<Option<T>> {
    let record = connection
        .query_row(
            &format!("SELECT record FROM {table} WHERE id = ?1"),
            [id],
            |row| {
                let json: String = row.get(0)?;
                serde_json::from_str(&json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            },
        )
        .optional()?;
    Ok(record)
}

fn put_json_record<T: Serialize>(
    connection: &Connection,
    table: &str,
    id: &str,
    record: &T,
) -> StoreResult<()> {
    connection.execute(
        &format!(
            "INSERT INTO {table}(id, record) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record"
        ),
        params![id, serde_json::to_string(record)?],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AgentRecord, TaskRecord};

    #[test]
    fn agent_round_trip_keeps_unknown_json_fields_and_number_kinds() {
        let source = r#"{
            "id":"agent-1", "rootId":"agent-1", "epoch":2, "status":"idle",
            "futureObject":{"number":1.5,"flag":true}, "futureInteger":9007199254740993
        }"#;
        let record: AgentRecord = serde_json::from_str(source).unwrap();
        let rewritten = serde_json::to_value(record).unwrap();
        assert_eq!(rewritten["futureObject"]["number"].as_f64(), Some(1.5));
        assert_eq!(rewritten["futureObject"]["flag"].as_bool(), Some(true));
        assert_eq!(rewritten["futureInteger"].as_i64(), Some(9007199254740993));
    }

    #[test]
    fn task_round_trip_keeps_boolean_distinct_from_integer() {
        let source = r#"{
            "id":"task-1", "rootId":"agent-1", "status":"ready", "title":"T",
            "description":"", "owner":null, "created":1.25, "updated":1.5,
            "createdBy":"agent-1", "version":3, "dependencies":[], "decisions":[],
            "results":[], "fixtureFlag":true, "fixtureNumber":1
        }"#;
        let record: TaskRecord = serde_json::from_str(source).unwrap();
        let rewritten = serde_json::to_value(record).unwrap();
        assert_eq!(rewritten["fixtureFlag"].as_bool(), Some(true));
        assert_eq!(rewritten["fixtureNumber"].as_i64(), Some(1));
    }
}
