use std::marker::PhantomData;

use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;
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

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
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
                Ok(Self::parse(&value))
            }
        }

        impl $name {
            fn parse(value: &str) -> Self {
                match value {
                    $($value => Self::$variant,)+
                    _ => Self::Other(value.to_owned()),
                }
            }

            fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $value,)+
                    Self::Other(value) => value,
                }
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

#[derive(Clone, Debug, PartialEq)]
pub struct AgentRecord {
    id: AgentId,
    value: Value,
}

impl AgentRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let id = record_id(&value)?;
        Ok(Self {
            id: AgentId::from(id),
            value,
        })
    }

    pub fn id(&self) -> &AgentId {
        &self.id
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.value.get(key)
    }

    pub fn set(&mut self, key: impl Into<String>, value: Value) {
        if let Some(object) = self.value.as_object_mut() {
            object.insert(key.into(), value);
        }
    }

    pub fn root_id(&self) -> Option<AgentId> {
        self.value
            .get("rootId")
            .and_then(Value::as_str)
            .map(AgentId::from)
    }

    pub fn epoch(&self) -> Option<i64> {
        self.value.get("epoch").and_then(Value::as_i64)
    }

    pub fn status(&self) -> Option<AgentStatus> {
        self.value
            .get("status")
            .and_then(Value::as_str)
            .map(AgentStatus::parse)
    }

    pub fn set_status(&mut self, status: AgentStatus) {
        if let Some(object) = self.value.as_object_mut() {
            object.insert(
                "status".to_owned(),
                Value::String(status.as_str().to_owned()),
            );
        }
    }
}

impl Serialize for AgentRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AgentRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

stored_string_enum!(WorkStatus {
    Ready => "ready",
    Running => "running",
    Blocked => "blocked",
    Review => "review",
    Accepted => "accepted",
    Cancelled => "cancelled",
});

#[derive(Clone, Debug, PartialEq)]
pub struct TaskRecord {
    id: TaskId,
    value: Value,
}

impl TaskRecord {
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let id = record_id(&value)?;
        Ok(Self {
            id: TaskId::from(id),
            value,
        })
    }

    pub fn id(&self) -> &TaskId {
        &self.id
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.value.get(key)
    }

    pub fn set(&mut self, key: impl Into<String>, value: Value) {
        if let Some(object) = self.value.as_object_mut() {
            object.insert(key.into(), value);
        }
    }

    pub fn status(&self) -> Option<WorkStatus> {
        self.value
            .get("status")
            .and_then(Value::as_str)
            .map(WorkStatus::parse)
    }

    pub fn title(&self) -> Option<&str> {
        self.value.get("title").and_then(Value::as_str)
    }

    pub fn set_status(&mut self, status: WorkStatus) {
        if let Some(object) = self.value.as_object_mut() {
            object.insert(
                "status".to_owned(),
                Value::String(status.as_str().to_owned()),
            );
        }
    }
}

impl Serialize for TaskRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TaskRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

fn record_id(value: &Value) -> Result<String, serde_json::Error> {
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| serde::de::Error::custom("record id must be a string"))
}

/// A JSON object stored in an `id`/`record` table.
pub trait JsonRecord: Serialize + DeserializeOwned {
    type Id: AsRef<str>;

    const TABLE: &'static str;
    const SCHEMA: &'static str;

    fn id(&self) -> &Self::Id;
}

impl JsonRecord for AgentRecord {
    type Id = AgentId;

    const TABLE: &'static str = "runtime_agents";
    const SCHEMA: &'static str = "AgentRecordResponse";

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl JsonRecord for TaskRecord {
    type Id = TaskId;

    const TABLE: &'static str = "runtime_tasks";
    const SCHEMA: &'static str = "WorkItem";

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

/// A page preserves SQLite row insertion order and returns a cursor only when more rows exist.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordPage<R> {
    pub records: Vec<R>,
    pub next_cursor: Option<i64>,
}

/// Typed access to one JSON-record table.
pub struct Table<'connection, R> {
    connection: &'connection Connection,
    record: PhantomData<fn() -> R>,
}

impl<'connection, R: JsonRecord> Table<'connection, R> {
    pub fn new(connection: &'connection Connection) -> Self {
        Self {
            connection,
            record: PhantomData,
        }
    }

    pub fn get(&self, id: &R::Id) -> StoreResult<Option<R>> {
        let sql = format!("SELECT record FROM {} WHERE id = ?1", quoted_table::<R>());
        let record = self
            .connection
            .query_row(&sql, [id.as_ref()], |row| row.get::<_, String>(0))
            .optional()?;
        record
            .map(|record| decode_record::<R>(id.as_ref(), &record))
            .transpose()
    }

    pub fn list(&self) -> StoreResult<Vec<R>> {
        let sql = format!(
            "SELECT id,record FROM {} ORDER BY rowid",
            quoted_table::<R>()
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(id, record)| decode_record::<R>(&id, &record))
            .collect()
    }

    pub fn page(&self, after_rowid: i64, limit: usize) -> StoreResult<RecordPage<R>> {
        let sql = format!(
            "SELECT rowid,id,record FROM {} WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            quoted_table::<R>()
        );
        let limit = limit.clamp(1, 500);
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params![after_rowid, (limit + 1) as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut decoded = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(rowid, id, record)| {
                decode_record::<R>(&id, &record).map(|record| (rowid, record))
            })
            .collect::<StoreResult<Vec<_>>>()?;
        let has_more = decoded.len() > limit;
        if has_more {
            decoded.pop();
        }
        let next_cursor = has_more
            .then(|| decoded.last().map(|(rowid, _)| *rowid))
            .flatten();
        Ok(RecordPage {
            records: decoded.into_iter().map(|(_, record)| record).collect(),
            next_cursor,
        })
    }

    pub fn put(&self, record: &R) -> StoreResult<()> {
        let encoded = encode_record(record)?;
        let sql = format!(
            "INSERT INTO {}(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            quoted_table::<R>()
        );
        self.connection
            .execute(&sql, params![record.id().as_ref(), encoded])?;
        Ok(())
    }

    pub fn insert_new(&self, record: &R) -> StoreResult<bool> {
        let encoded = encode_record(record)?;
        let sql = format!(
            "INSERT OR IGNORE INTO {}(id,record) VALUES (?1,?2)",
            quoted_table::<R>()
        );
        let inserted = self
            .connection
            .execute(&sql, params![record.id().as_ref(), encoded])?;
        Ok(inserted == 1)
    }
}

fn quoted_table<R: JsonRecord>() -> String {
    format!("\"{}\"", R::TABLE.replace('"', "\"\""))
}

fn decode_record<R: JsonRecord>(row_id: &str, encoded: &str) -> StoreResult<R> {
    let invalid = || crate::StoreError::InvalidStoredObject {
        table: R::TABLE,
        id: row_id.to_owned(),
    };
    let value: Value = serde_json::from_str(encoded).map_err(|_| invalid())?;
    if !studio_runtime_schema::validate(R::SCHEMA, &value)
        .map_err(crate::StoreError::SchemaValidation)?
    {
        return Err(invalid());
    }
    let record = serde_json::from_value::<R>(value).map_err(|_| invalid())?;
    if record.id().as_ref() != row_id {
        return Err(invalid());
    }
    Ok(record)
}

fn encode_record<R: JsonRecord>(record: &R) -> StoreResult<String> {
    let value = serde_json::to_value(record)?;
    let invalid = || crate::StoreError::InvalidStoredObject {
        table: R::TABLE,
        id: record.id().as_ref().to_owned(),
    };
    if value.get("id").and_then(Value::as_str) != Some(record.id().as_ref())
        || !studio_runtime_schema::validate(R::SCHEMA, &value)
            .map_err(crate::StoreError::SchemaValidation)?
    {
        return Err(invalid());
    }
    Ok(serde_json::to_string(&value)?)
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

#[cfg(test)]
mod tests {
    use super::{AgentId, AgentRecord, AgentStatus, Table, TaskRecord, WorkStatus};
    use crate::StoreError;
    use rusqlite::Connection;
    use serde_json::json;

    fn agent(value: serde_json::Value) -> AgentRecord {
        AgentRecord::from_value(value).expect("agent record has a string id")
    }

    fn task(value: serde_json::Value) -> TaskRecord {
        TaskRecord::from_value(value).expect("task record has a string id")
    }

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("agent table creates");
        connection
    }

    #[test]
    fn agent_round_trip_keeps_unknown_json_fields_and_number_kinds() {
        let value = json!({
            "id":"agent-1", "rootId":"agent-1", "epoch":2, "status":"idle",
            "futureObject":{"number":1.5,"flag":true}, "futureInteger":9007199254740993_i64
        });
        let record = agent(value.clone());
        let rewritten = serde_json::to_value(record).expect("agent record serializes");
        assert_eq!(rewritten["futureObject"]["number"].as_f64(), Some(1.5));
        assert_eq!(rewritten["futureObject"]["flag"].as_bool(), Some(true));
        assert_eq!(rewritten["futureInteger"].as_i64(), Some(9007199254740993));
        assert_eq!(rewritten, value);
    }

    #[test]
    fn task_round_trip_keeps_boolean_distinct_from_integer() {
        let record = task(json!({
            "id":"task-1", "rootId":"agent-1", "status":"ready", "title":"T",
            "description":"", "owner":null, "created":1.25, "updated":1.5,
            "createdBy":"agent-1", "version":3, "dependencies":[], "decisions":[],
            "results":[], "fixtureFlag":true, "fixtureNumber":1
        }));
        let rewritten = serde_json::to_value(record).expect("task record serializes");
        assert_eq!(rewritten["fixtureFlag"].as_bool(), Some(true));
        assert_eq!(rewritten["fixtureNumber"].as_i64(), Some(1));
    }

    #[test]
    fn unknown_agent_status_and_fields_survive_rewrite() {
        let value = json!({"id":"agent-1","rootId":"agent-1","epoch":4,
            "status":"future-agent-state","future":{"enabled":true,"count":9007199254740993_i64}});
        let record = agent(value.clone());
        assert_eq!(
            record.status(),
            Some(AgentStatus::Other("future-agent-state".into()))
        );
        assert_eq!(
            serde_json::to_value(record).expect("record serializes"),
            value
        );
    }

    #[test]
    fn unknown_work_status_and_fields_survive_rewrite() {
        let value = json!({"id":"task-1","rootId":"agent-1","status":"future_work_state",
            "title":"T","description":"","owner":null,"created":1.25,"updated":1.5,
            "createdBy":"agent-1","version":3,"dependencies":[],"decisions":[],"results":[],
            "future":{"enabled":false,"count":9007199254740993_i64}});
        let record = task(value.clone());
        assert_eq!(
            record.status(),
            Some(WorkStatus::Other("future_work_state".into()))
        );
        assert_eq!(
            serde_json::to_value(record).expect("record serializes"),
            value
        );
    }

    #[test]
    fn agent_table_put_get_insert_new_list_and_page_share_one_record_layer() {
        let connection = database();
        let table = Table::<AgentRecord>::new(&connection);
        let first = agent(json!({"id":"a1","rootId":"a1","epoch":0,"status":"idle"}));
        let second = agent(json!({"id":"a2","rootId":"a2","epoch":0,"status":"queued"}));

        assert!(table.insert_new(&first).expect("first insert succeeds"));
        assert!(
            !table
                .insert_new(&first)
                .expect("duplicate insert is ignored")
        );
        assert!(table.insert_new(&second).expect("second insert succeeds"));
        assert_eq!(
            table
                .get(&AgentId::from("a2"))
                .expect("agent reads")
                .expect("agent exists after insertion")
                .id(),
            &AgentId::from("a2")
        );
        let first_page = table.page(0, 1).expect("first page reads");
        assert_eq!(first_page.records.len(), 1);
        let cursor = first_page.next_cursor.expect("another row remains");
        let last_page = table.page(cursor, 1).expect("last page reads");
        assert_eq!(last_page.records[0].id().as_ref(), "a2");
        assert_eq!(last_page.next_cursor, None);

        let mut changed = first;
        changed.set_status(AgentStatus::Running);
        table
            .put(&changed)
            .expect("put updates the existing record");
        assert_eq!(
            table.list().expect("records list")[0].status(),
            Some(AgentStatus::Running)
        );
    }

    #[test]
    fn table_refuses_invalid_json_and_row_id_mismatches() {
        let connection = database();
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('bad-json','{')",
                [],
            )
            .expect("malformed fixture inserts");
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('outer-id','{\"id\":\"inner-id\",\"rootId\":\"inner-id\",\"epoch\":0,\"status\":\"idle\"}')",
                [],
            )
            .expect("mismatched fixture inserts");
        let table = Table::<AgentRecord>::new(&connection);

        for id in ["bad-json", "outer-id"] {
            assert!(matches!(
                table.get(&AgentId::from(id)).expect_err("invalid record is refused"),
                StoreError::InvalidStoredObject { table: "runtime_agents", id: ref bad_id }
                    if bad_id == id
            ));
        }
    }

    #[test]
    fn table_rejects_schema_invalid_reads_and_writes_with_table_and_id() {
        let connection = database();
        let table = Table::<AgentRecord>::new(&connection);
        let invalid = agent(json!({
            "id":"invalid-status","rootId":"invalid-status","epoch":0,
            "status":"future-agent-state"
        }));
        assert!(matches!(
            table.put(&invalid).expect_err("invalid record is not written"),
            StoreError::InvalidStoredObject { table: "runtime_agents", id }
                if id == "invalid-status"
        ));
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM runtime_agents", [], |row| row
                    .get::<_, i64>(0))
                .expect("table count reads"),
            0
        );

        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('invalid-status','{\"id\":\"invalid-status\",\"rootId\":\"invalid-status\",\"epoch\":0,\"status\":\"future-agent-state\"}')",
                [],
            )
            .expect("invalid record fixture inserts");
        assert!(matches!(
            table
                .list()
                .expect_err("schema-invalid record is not returned"),
            StoreError::InvalidStoredObject { table: "runtime_agents", id }
                if id == "invalid-status"
        ));
    }
}
