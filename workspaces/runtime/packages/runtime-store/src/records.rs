use std::marker::PhantomData;

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, params, params_from_iter};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::StoreResult;

macro_rules! row_key_accessor {
    ($name:ident, $id:ty, PayloadId) => {
        impl $name {
            pub fn id(&self) -> &$id {
                self.row_key.as_ref().expect("payload-id record has no id")
            }
        }
    };
    ($name:ident, $id:ty, External) => {};
}

macro_rules! record_newtype {
    ($name:ident, $id:ty, table = $table:literal, schema = $schema:literal,
     key = $key:ident, optional = $optional:literal, query = $query:ident) => {
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name {
            row_key: Option<$id>,
            value: Value,
        }

        impl $name {
            pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
                let row_key = match RecordKeyMode::$key {
                    RecordKeyMode::PayloadId => Some(<$id>::from(record_id(&value)?)),
                    RecordKeyMode::External => None,
                };
                Ok(Self { row_key, value })
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
        }

        row_key_accessor!($name, $id, $key);

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                self.value.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Self::from_value(Value::deserialize(deserializer)?)
                    .map_err(serde::de::Error::custom)
            }
        }

        impl JsonRecord for $name {
            type Id = $id;

            const TABLE: &'static str = $table;
            const SCHEMA: &'static str = $schema;
            const KEY_MODE: RecordKeyMode = RecordKeyMode::$key;
            const OPTIONAL: bool = $optional;
            const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::$query;

            fn record_key(&self) -> Option<&Self::Id> {
                self.row_key.as_ref()
            }
        }
    };
}

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
string_id!(TranscriptItemId);
string_id!(TranscriptAssetId);

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

record_newtype!(
    AgentRecord,
    AgentId,
    table = "runtime_agents",
    schema = "AgentRecordResponse",
    key = PayloadId,
    optional = false,
    query = None
);
impl AgentRecord {
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

stored_string_enum!(WorkStatus {
    Ready => "ready",
    Running => "running",
    Blocked => "blocked",
    Review => "review",
    Accepted => "accepted",
    Cancelled => "cancelled",
});

record_newtype!(
    TaskRecord,
    TaskId,
    table = "runtime_tasks",
    schema = "TaskDetailResponse",
    key = PayloadId,
    optional = false,
    query = JsonAgent
);
record_newtype!(
    WorkItemRecord,
    TaskId,
    table = "runtime_work",
    schema = "WorkItem",
    key = PayloadId,
    optional = true,
    query = None
);
record_newtype!(
    RuleRecord,
    String,
    table = "runtime_rules",
    schema = "RuleRecordResponse",
    key = PayloadId,
    optional = false,
    query = None
);
record_newtype!(
    TranscriptItemRecord,
    TranscriptItemId,
    table = "runtime_items",
    schema = "PersistedTranscriptRecord",
    key = PayloadId,
    optional = false,
    query = PhysicalAgent
);
record_newtype!(
    TranscriptAssetRecord,
    TranscriptAssetId,
    table = "runtime_assets",
    schema = "PersistedTranscriptAsset",
    key = PayloadId,
    optional = true,
    query = JsonAgent
);
record_newtype!(
    EventMetadataRecord,
    QueueEventId,
    table = "runtime_event_meta",
    schema = "RuntimeEventMetadataRecord",
    key = External,
    optional = true,
    query = None
);

impl WorkItemRecord {
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

fn record_id(value: &Value) -> Result<String, serde_json::Error> {
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| serde::de::Error::custom("record id must be a string"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordKeyMode {
    PayloadId,
    External,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordQueryShape {
    None,
    JsonAgent,
    PhysicalAgent,
}

/// The only row filters shared by JSON-record tables.
#[derive(Clone, Copy, Debug)]
pub enum RecordPredicate<'a> {
    PhysicalAgentEquals(&'a str),
    PhysicalAgentIn(&'a [String]),
    JsonAgentEquals(&'a str),
    JsonAgentIn(&'a [String]),
}

/// Supported orderings have fixed SQL expressions and tie-breakers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOrder {
    RowId,
    PhysicalCreatedDescIdDesc,
    PhysicalCreatedAscIdAsc,
    JsonCreatedDescIdDesc,
    JsonEffectiveCreatedAscIdAsc,
}

/// A JSON object stored in an id/record table.
pub trait JsonRecord: Serialize + DeserializeOwned {
    type Id: AsRef<str> + From<String>;

    const TABLE: &'static str;
    const SCHEMA: &'static str;
    const KEY_MODE: RecordKeyMode;
    const OPTIONAL: bool;
    const QUERY_SHAPE: RecordQueryShape;

    fn record_key(&self) -> Option<&Self::Id>;
}

/// Typed access to one id/record JSON table.
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
        self.get_matching(id, None)
    }

    pub fn get_matching(
        &self,
        id: &R::Id,
        predicate: Option<RecordPredicate<'_>>,
    ) -> StoreResult<Option<R>> {
        ensure_query::<R>(predicate, RecordOrder::RowId)?;
        if !self.table_is_present()? {
            return Ok(None);
        }
        let (where_sql, mut values) = predicate_sql(predicate);
        let id_predicate = where_sql.map_or_else(String::new, |sql| format!(" AND {sql}"));
        values.insert(0, SqlValue::Text(id.as_ref().to_owned()));
        let sql = format!(
            "SELECT record FROM {} WHERE id = ?1{id_predicate}",
            quoted_table::<R>()
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let encoded = stored_text(row.get_ref(0)?, R::TABLE, id.as_ref())?;
        decode_record::<R>(id.as_ref(), &encoded).map(Some)
    }

    /// Returns None only when an optional table is absent; an empty present
    /// table returns Some(Vec::new()).
    pub fn list(&self) -> StoreResult<Option<Vec<R>>> {
        self.list_matching(None, RecordOrder::RowId)
    }

    pub fn list_matching(
        &self,
        predicate: Option<RecordPredicate<'_>>,
        order: RecordOrder,
    ) -> StoreResult<Option<Vec<R>>> {
        ensure_query::<R>(predicate, order)?;
        if !self.table_is_present()? {
            return Ok(None);
        }
        let (where_sql, values) = predicate_sql(predicate);
        let where_clause = where_sql.map_or_else(String::new, |sql| format!(" WHERE {sql}"));
        let sql = format!(
            "SELECT id,record FROM {}{where_clause} ORDER BY {}",
            quoted_table::<R>(),
            order.sql()
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let id = stored_text(row.get_ref(0)?, R::TABLE, "<invalid id>")?;
            let encoded = stored_text(row.get_ref(1)?, R::TABLE, &id)?;
            records.push(decode_record::<R>(&id, &encoded)?);
        }
        Ok(Some(records))
    }

    pub fn put(&self, record: &R) -> StoreResult<()> {
        if R::KEY_MODE != RecordKeyMode::PayloadId {
            return Err(crate::StoreError::RecordContract(
                "external-key records must be written with put_at",
            ));
        }
        let key = record
            .record_key()
            .ok_or(crate::StoreError::RecordContract(
                "payload-id record has no cached id",
            ))?;
        self.put_at_key(key.as_ref(), record, true).map(|_| ())
    }

    /// Upsert a payload without adding the external table key to its JSON.
    pub fn put_at(&self, id: &R::Id, record: &R) -> StoreResult<()> {
        if R::KEY_MODE != RecordKeyMode::External {
            return Err(crate::StoreError::RecordContract(
                "put_at is only valid for external-key records",
            ));
        }
        self.put_at_key(id.as_ref(), record, true).map(|_| ())
    }

    fn put_at_key(&self, id: &str, record: &R, update: bool) -> StoreResult<bool> {
        let encoded = encode_record(record, id)?;
        let conflict = if update {
            "DO UPDATE SET record=excluded.record"
        } else {
            "DO NOTHING"
        };
        let sql = format!(
            "INSERT INTO {}(id,record) VALUES (?1,?2) ON CONFLICT(id) {conflict}",
            quoted_table::<R>()
        );
        let changed = self.connection.execute(&sql, params![id, encoded])?;
        Ok(changed == 1)
    }

    pub fn insert_new(&self, record: &R) -> StoreResult<bool> {
        if R::KEY_MODE != RecordKeyMode::PayloadId {
            return Err(crate::StoreError::RecordContract(
                "external-key records must be written with insert_new_at",
            ));
        }
        let key = record
            .record_key()
            .ok_or(crate::StoreError::RecordContract(
                "payload-id record has no cached id",
            ))?;
        self.put_at_key(key.as_ref(), record, false)
    }

    pub fn insert_new_at(&self, id: &R::Id, record: &R) -> StoreResult<bool> {
        if R::KEY_MODE != RecordKeyMode::External {
            return Err(crate::StoreError::RecordContract(
                "insert_new_at is only valid for external-key records",
            ));
        }
        self.put_at_key(id.as_ref(), record, false)
    }

    fn table_is_present(&self) -> StoreResult<bool> {
        if !R::OPTIONAL {
            return Ok(true);
        }
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE)",
            [R::TABLE],
            |row| row.get(0),
        )?)
    }
}

impl RecordOrder {
    fn sql(self) -> &'static str {
        match self {
            Self::RowId => "rowid",
            Self::PhysicalCreatedDescIdDesc => "created DESC,id DESC",
            Self::PhysicalCreatedAscIdAsc => "created ASC,id ASC",
            Self::JsonCreatedDescIdDesc => "json_extract(record,'$.created') DESC,id DESC",
            Self::JsonEffectiveCreatedAscIdAsc => {
                "CASE WHEN COALESCE(json_extract(record,'$.finished'),0) > \
                 COALESCE(json_extract(record,'$.created'),0) \
                 THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END ASC,id ASC"
            }
        }
    }
}

fn ensure_query<R: JsonRecord>(
    predicate: Option<RecordPredicate<'_>>,
    order: RecordOrder,
) -> StoreResult<()> {
    let valid_filter = match predicate {
        None => true,
        Some(RecordPredicate::PhysicalAgentEquals(_) | RecordPredicate::PhysicalAgentIn(_)) => {
            R::QUERY_SHAPE == RecordQueryShape::PhysicalAgent
        }
        Some(RecordPredicate::JsonAgentEquals(_) | RecordPredicate::JsonAgentIn(_)) => {
            R::QUERY_SHAPE == RecordQueryShape::JsonAgent
        }
    };
    let valid_order = match order {
        RecordOrder::RowId => true,
        RecordOrder::PhysicalCreatedDescIdDesc | RecordOrder::PhysicalCreatedAscIdAsc => {
            R::QUERY_SHAPE == RecordQueryShape::PhysicalAgent
        }
        RecordOrder::JsonCreatedDescIdDesc | RecordOrder::JsonEffectiveCreatedAscIdAsc => {
            R::QUERY_SHAPE == RecordQueryShape::JsonAgent
        }
    };
    if valid_filter && valid_order {
        Ok(())
    } else {
        Err(crate::StoreError::RecordContract(
            "query choice is not declared by this record type",
        ))
    }
}

fn predicate_sql(predicate: Option<RecordPredicate<'_>>) -> (Option<String>, Vec<SqlValue>) {
    match predicate {
        None => (None, Vec::new()),
        Some(RecordPredicate::PhysicalAgentEquals(value)) => (
            Some("agent=?".to_owned()),
            vec![SqlValue::Text(value.to_owned())],
        ),
        Some(RecordPredicate::JsonAgentEquals(value)) => (
            Some("json_extract(record,'$.agent')=?".to_owned()),
            vec![SqlValue::Text(value.to_owned())],
        ),
        Some(RecordPredicate::PhysicalAgentIn(values)) => in_predicate("agent", values),
        Some(RecordPredicate::JsonAgentIn(values)) => {
            in_predicate("json_extract(record,'$.agent')", values)
        }
    }
}

fn in_predicate(column: &str, values: &[String]) -> (Option<String>, Vec<SqlValue>) {
    if values.is_empty() {
        return (Some("0".to_owned()), Vec::new());
    }
    let marks = vec!["?"; values.len()].join(",");
    (
        Some(format!("{column} IN ({marks})")),
        values.iter().cloned().map(SqlValue::Text).collect(),
    )
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
    if R::KEY_MODE == RecordKeyMode::PayloadId
        && record.record_key().map(AsRef::as_ref) != Some(row_id)
    {
        return Err(invalid());
    }
    Ok(record)
}

fn encode_record<R: JsonRecord>(record: &R, row_id: &str) -> StoreResult<String> {
    let value = serde_json::to_value(record)?;
    let invalid = || crate::StoreError::InvalidStoredObject {
        table: R::TABLE,
        id: row_id.to_owned(),
    };
    if (R::KEY_MODE == RecordKeyMode::PayloadId
        && (record.record_key().map(AsRef::as_ref) != Some(row_id)
            || value.get("id").and_then(Value::as_str) != Some(row_id)))
        || !studio_runtime_schema::validate(R::SCHEMA, &value)
            .map_err(crate::StoreError::SchemaValidation)?
    {
        return Err(invalid());
    }
    Ok(serde_json::to_string(&value)?)
}

fn stored_text(value: ValueRef<'_>, table: &'static str, id: &str) -> StoreResult<String> {
    match value {
        ValueRef::Text(bytes) => {
            String::from_utf8(bytes.to_vec()).map_err(|_| crate::StoreError::InvalidStoredObject {
                table,
                id: id.to_owned(),
            })
        }
        _ => Err(crate::StoreError::InvalidStoredObject {
            table,
            id: id.to_owned(),
        }),
    }
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
    use super::{
        AgentId, AgentRecord, AgentStatus, EventMetadataRecord, QueueEventId, RecordOrder,
        RecordPredicate, Table, TaskId, TaskRecord, TranscriptAssetId, TranscriptAssetRecord,
        TranscriptItemId, TranscriptItemRecord, WorkItemRecord, WorkStatus,
    };
    use crate::StoreError;
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn agent(value: Value) -> AgentRecord {
        AgentRecord::from_value(value).expect("agent record has a string id")
    }

    fn task(value: Value) -> TaskRecord {
        TaskRecord::from_value(value).expect("runtime task has a string id")
    }

    fn work(value: Value) -> WorkItemRecord {
        WorkItemRecord::from_value(value).expect("work item has a string id")
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
    fn runtime_task_and_work_item_use_distinct_exported_schemas() {
        let runtime_task = task(json!({
            "id":"task-1", "agent":"agent-1", "kind":"command", "status":"completed",
            "created":1.25, "interactive":true, "exitCode":7
        }));
        let rewritten = serde_json::to_value(runtime_task).expect("runtime task serializes");
        assert_eq!(rewritten["interactive"], json!(true));
        assert_eq!(rewritten["exitCode"].as_i64(), Some(7));

        let work = work(json!({
            "id":"work-1", "rootId":"agent-1", "status":"ready", "title":"T",
            "created":1.25, "updated":1.5, "version":3, "futureField":true
        }));
        assert_eq!(work.status(), Some(WorkStatus::Ready));
        assert_eq!(
            serde_json::to_value(work).expect("work item serializes")["futureField"],
            json!(true)
        );
    }

    #[test]
    fn unknown_agent_status_and_fields_survive_carrier_rewrite() {
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
    fn table_upserts_reads_lists_and_inserts_only_once() {
        let connection = database();
        let table = Table::<AgentRecord>::new(&connection);
        let first = agent(json!({"id":"a1","rootId":"a1","epoch":0,"status":"idle"}));
        let second = agent(json!({"id":"a2","rootId":"a2","epoch":0,"status":"queued"}));

        assert!(table.insert_new(&first).expect("first insert succeeds"));
        assert!(!table.insert_new(&first).expect("duplicate id is ignored"));
        assert!(table.insert_new(&second).expect("second insert succeeds"));
        assert_eq!(
            table
                .get(&AgentId::from("a2"))
                .expect("agent reads")
                .expect("agent exists")
                .id(),
            &AgentId::from("a2")
        );

        let mut changed = first;
        changed.set_status(AgentStatus::Running);
        table
            .put(&changed)
            .expect("put updates the existing record");
        assert_eq!(
            table
                .list()
                .expect("records list")
                .expect("required table exists")[0]
                .status(),
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
    fn non_text_and_invalid_utf8_records_are_invalid_stored_objects() {
        let connection = database();
        connection
            .execute_batch(
                "INSERT INTO runtime_agents(id,record) VALUES('blob',x'7b7d');
                 INSERT INTO runtime_agents(id,record) VALUES('invalid-utf8',CAST(x'ff' AS TEXT));",
            )
            .expect("corrupt record fixtures insert");
        let table = Table::<AgentRecord>::new(&connection);

        for id in ["blob", "invalid-utf8"] {
            assert!(matches!(
                table.get(&AgentId::from(id)).expect_err("bad storage type is rejected"),
                StoreError::InvalidStoredObject { table: "runtime_agents", id: ref bad_id }
                    if bad_id == id
            ));
        }
        assert!(matches!(
            table.list().expect_err("bad list row is rejected"),
            StoreError::InvalidStoredObject { table: "runtime_agents", id }
                if id == "blob"
        ));
    }

    #[test]
    fn invalid_schema_reads_and_writes_include_the_record_key() {
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
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('invalid-status','{\"id\":\"invalid-status\",\"rootId\":\"invalid-status\",\"epoch\":0,\"status\":\"future-agent-state\"}')",
                [],
            )
            .expect("invalid record fixture inserts");
        assert!(matches!(
            table.list().expect_err("schema-invalid record is not returned"),
            StoreError::InvalidStoredObject { table: "runtime_agents", id }
                if id == "invalid-status"
        ));
    }

    #[test]
    fn optional_tables_distinguish_absent_from_present_empty_and_keep_sql_errors() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        let table = Table::<WorkItemRecord>::new(&connection);
        assert_eq!(
            table.get(&TaskId::from("missing")).expect("optional read"),
            None
        );
        assert_eq!(table.list().expect("absent optional list"), None);

        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("optional table creates");
        assert_eq!(
            table.list().expect("present optional list"),
            Some(Vec::new())
        );

        let malformed = Connection::open_in_memory().expect("second memory database opens");
        malformed
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY);")
            .expect("malformed optional table creates");
        assert!(matches!(
            Table::<WorkItemRecord>::new(&malformed)
                .list()
                .expect_err("present table SQL failure propagates"),
            StoreError::Sqlite(_)
        ));
    }

    #[test]
    fn external_keys_do_not_become_json_fields() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
            )
            .expect("optional event metadata table creates");
        let table = Table::<EventMetadataRecord>::new(&connection);
        let key = QueueEventId::from("event-1");
        let value = EventMetadataRecord::from_value(json!({"timing":{"enqueuedAt":9}}))
            .expect("metadata is an object");
        assert!(
            table
                .insert_new_at(&key, &value)
                .expect("first keyed insert")
        );
        assert!(
            !table
                .insert_new_at(&key, &value)
                .expect("duplicate keyed insert")
        );
        assert_eq!(
            table
                .get(&key)
                .expect("external key read")
                .expect("metadata exists")
                .value(),
            &json!({"timing":{"enqueuedAt":9}})
        );
        let stored: String = connection
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id='event-1'",
                [],
                |row| row.get(0),
            )
            .expect("record reads");
        let value = serde_json::from_str::<Value>(&stored).expect("stored metadata parses");
        assert_eq!(value, json!({"timing":{"enqueuedAt":9}}));
        assert!(value.get("id").is_none());
    }

    #[test]
    fn persisted_asset_table_keeps_path_and_extension_fields() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        let table = Table::<TranscriptAssetRecord>::new(&connection);
        assert_eq!(table.list().expect("absent assets table"), None);
        connection
            .execute_batch("CREATE TABLE runtime_assets(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("assets table creates");

        let asset = TranscriptAssetRecord::from_value(json!({
            "id":"asset-1", "agent":"agent-1", "name":"notes.txt",
            "path":"/private/uploads/asset-1/notes.txt", "mime":"text/plain",
            "image":false, "size":5, "hash":"sha256:abc", "created":1.5,
            "futureField":{"retained":true}
        }))
        .expect("asset has an id");
        table
            .put(&asset)
            .expect("persisted asset validates and writes");

        let read = table
            .get(&TranscriptAssetId::from("asset-1"))
            .expect("asset reads")
            .expect("asset exists");
        assert_eq!(
            read.get("path").and_then(Value::as_str),
            Some("/private/uploads/asset-1/notes.txt")
        );
        assert_eq!(read.value()["futureField"]["retained"], Value::Bool(true));

        let invalid = TranscriptAssetRecord::from_value(json!({
            "id":"bad-asset", "agent":"agent-1", "name":"notes.txt",
            "mime":"text/plain", "image":false, "size":5,
            "hash":"sha256:abc", "created":1.5
        }))
        .expect("invalid schema fixture has an id");
        assert!(matches!(
            table.put(&invalid).expect_err("path is required for persisted asset"),
            StoreError::InvalidStoredObject { table: "runtime_assets", id }
                if id == "bad-asset"
        ));
    }

    #[test]
    fn insert_new_only_ignores_the_declared_id_conflict() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL CHECK(id != 'blocked'));",
            )
            .expect("checked table creates");
        let table = Table::<AgentRecord>::new(&connection);
        let blocked = agent(json!({"id":"blocked","rootId":"blocked","epoch":0,"status":"idle"}));
        assert!(matches!(
            table.insert_new(&blocked),
            Err(StoreError::Sqlite(_))
        ));
    }

    #[test]
    fn record_queries_use_only_declared_filters_and_fixed_orderings() {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,created REAL,record TEXT NOT NULL);
                 INSERT INTO runtime_items VALUES('i1','a',2,'{\"id\":\"i1\",\"agent\":\"a\",\"text\":\"one\"}');
                 INSERT INTO runtime_items VALUES('i2','b',4,'{\"id\":\"i2\",\"agent\":\"b\",\"text\":\"two\"}');
                 INSERT INTO runtime_items VALUES('i3','a',3,'{\"id\":\"i3\",\"agent\":\"a\",\"text\":\"three\"}');",
            )
            .expect("transcript table creates");
        let table = Table::<TranscriptItemRecord>::new(&connection);
        let records = table
            .list_matching(
                Some(RecordPredicate::PhysicalAgentEquals("a")),
                RecordOrder::PhysicalCreatedDescIdDesc,
            )
            .expect("physical owner query")
            .expect("required transcript table exists");
        assert_eq!(
            records
                .iter()
                .map(|record| record.id().as_ref())
                .collect::<Vec<_>>(),
            ["i3", "i1"]
        );
        assert!(
            table
                .get_matching(
                    &TranscriptItemId::from("i1"),
                    Some(RecordPredicate::PhysicalAgentEquals("b"))
                )
                .expect("id and owner query")
                .is_none()
        );

        connection
            .execute_batch(
                "CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 INSERT INTO runtime_tasks VALUES('t1','{\"id\":\"t1\",\"agent\":\"a\",\"kind\":\"command\",\"status\":\"completed\",\"created\":1,\"finished\":3}');
                 INSERT INTO runtime_tasks VALUES('t2','{\"id\":\"t2\",\"agent\":\"a\",\"kind\":\"command\",\"status\":\"completed\",\"created\":2}');
                 INSERT INTO runtime_tasks VALUES('t3','{\"id\":\"t3\",\"agent\":\"b\",\"kind\":\"command\",\"status\":\"completed\",\"created\":0}');",
            )
            .expect("runtime task table creates");
        let agents = vec!["a".to_owned()];
        let tasks = Table::<TaskRecord>::new(&connection)
            .list_matching(
                Some(RecordPredicate::JsonAgentIn(&agents)),
                RecordOrder::JsonEffectiveCreatedAscIdAsc,
            )
            .expect("JSON field filter")
            .expect("required runtime task table exists");
        assert_eq!(
            tasks
                .iter()
                .map(|record| record.id().as_ref())
                .collect::<Vec<_>>(),
            ["t2", "t1"]
        );
        assert!(matches!(
            Table::<AgentRecord>::new(&connection).list_matching(
                Some(RecordPredicate::JsonAgentIn(&agents)),
                RecordOrder::RowId
            ),
            Err(StoreError::RecordContract(_))
        ));
    }
}
