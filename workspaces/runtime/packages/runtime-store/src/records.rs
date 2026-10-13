use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Row, params};
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

macro_rules! stored_string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub enum $name {
            $($variant,)+
            Other(String),
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                match self {
                    $(Self::$variant => serializer.serialize_str($value),)+
                    Self::Other(value) => serializer.serialize_str(value),
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Ok(match value.as_str() {
                    $($value => Self::$variant,)+
                    _ => Self::Other(value),
                })
            }
        }
    };
}

stored_string_enum!(AgentStatus {
    Unknown => "unknown",
    Idle => "idle",
    Queued => "queued",
    Starting => "starting",
    Running => "running",
    Waiting => "waiting",
    Approval => "approval",
    Completed => "completed",
    Interrupted => "interrupted",
    Failed => "failed",
    Blocked => "blocked",
    Paused => "paused",
    Parked => "parked",
    Abandoned => "abandoned",
    CapacityRetry => "capacity-retry",
});

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

stored_string_enum!(WorkStatus {
    Ready => "ready",
    Running => "running",
    Blocked => "blocked",
    Review => "review",
    Accepted => "accepted",
    Cancelled => "cancelled",
});

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

#[derive(Clone, Copy, Debug, Default)]
pub struct TaskRepository;

#[derive(Clone, Copy, Debug, Default)]
pub struct QueueEventRepository;

#[derive(Clone, Copy, Debug, Default)]
pub struct MessageRepository;

pub(crate) fn get_json_record<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    table: &str,
    id: &str,
) -> StoreResult<Option<T>> {
    let record = connection
        .query_row(
            &format!("SELECT record FROM {table} WHERE id = ?1"),
            [id],
            |row| decode_json_record(row, 0),
        )
        .optional()?;
    Ok(record)
}

pub(crate) fn decode_json_record<T: serde::de::DeserializeOwned>(
    row: &Row<'_>,
    column: usize,
) -> rusqlite::Result<T> {
    let json: String = row.get(column)?;
    serde_json::from_str(&json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

pub(crate) fn get_raw_record(
    connection: &Connection,
    table: &str,
    id: &str,
) -> StoreResult<Option<String>> {
    connection
        .query_row(
            &format!("SELECT record FROM {table} WHERE id = ?1"),
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

pub(crate) fn put_json_record<T: Serialize>(
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
        let record: AgentRecord = serde_json::from_str(source).expect("agent record parses");
        let rewritten = serde_json::to_value(record).expect("agent record serializes");
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
        let record: TaskRecord = serde_json::from_str(source).expect("task record parses");
        let rewritten = serde_json::to_value(record).expect("task record serializes");
        assert_eq!(rewritten["fixtureFlag"].as_bool(), Some(true));
        assert_eq!(rewritten["fixtureNumber"].as_i64(), Some(1));
    }

    #[test]
    fn unknown_agent_status_and_fields_survive_rewrite() {
        let source = r#"{"id":"agent-1","rootId":"agent-1","epoch":4,
            "status":"future-agent-state","future":{"enabled":true,"count":9007199254740993}}"#;
        let record: AgentRecord = serde_json::from_str(source).expect("agent record parses");
        let rewritten = serde_json::to_value(record).expect("agent record serializes");
        assert_eq!(
            rewritten,
            serde_json::from_str::<serde_json::Value>(source).expect("json parses")
        );
    }

    #[test]
    fn unknown_work_status_and_fields_survive_rewrite() {
        let source = r#"{"id":"task-1","rootId":"agent-1","status":"future_work_state",
            "title":"T","description":"","owner":null,"created":1.25,"updated":1.5,
            "createdBy":"agent-1","version":3,"dependencies":[],"decisions":[],"results":[],
            "future":{"enabled":false,"count":9007199254740993}}"#;
        let record: TaskRecord = serde_json::from_str(source).expect("task record parses");
        let rewritten = serde_json::to_value(record).expect("task record serializes");
        assert_eq!(
            rewritten,
            serde_json::from_str::<serde_json::Value>(source).expect("json parses")
        );
    }
}
