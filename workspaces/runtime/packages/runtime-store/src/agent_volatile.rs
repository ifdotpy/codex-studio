//! Volatile agent fields, stored apart from the indexed agent record.
//!
//! Storage format of 2026-10-20 ([`migrate_2026_10_20`]). Provider
//! notifications change the fields in [`AGENT_VOLATILE_FIELDS`] many times per
//! turn. They live in [`AGENT_VOLATILE_TABLE`]: one row per agent and one
//! column per field, each holding that field's JSON value. A notification that
//! changes only these fields therefore leaves `runtime_agents` alone, so its
//! JSON expression indexes are not re-evaluated, its record is not rewritten
//! and its row-change observers are not woken. No index or SQL predicate may
//! read these fields from `runtime_agents.record`.
//!
//! Readers see one agent object: [`merge`] adds the stored fields to a decoded
//! record. Writers store through [`put`] and [`update`]: the record without these
//! fields is written only when it changed, and every field present in the
//! agent replaces its column. A field absent from a written agent keeps its
//! stored value, so a writer that read the record without merging cannot erase
//! it. No writer removes one of these fields; it stores JSON null instead.

use std::sync::LazyLock;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};

use crate::schema::{TableRequirement, TriggerRequirement};
use crate::{CachedSql, StoreError, StoreResult};

/// The table that holds the volatile fields of each agent, keyed by agent id.
pub const AGENT_VOLATILE_TABLE: &str = "runtime_agent_volatile";

/// The agent fields that hot provider notifications change: event counter and
/// time, activity and running tools, answer tail and last answer, context
/// window usage, and token counters. Each is a column of
/// [`AGENT_VOLATILE_TABLE`] with the field's JSON value.
pub const AGENT_VOLATILE_FIELDS: [&str; 9] = [
    "events",
    "lastEvent",
    "activity",
    "activeTools",
    "tail",
    "lastAnswer",
    "contextUsage",
    "tokensUsed",
    "tokenUsageAccounting",
];

/// Whether `key` is one of [`AGENT_VOLATILE_FIELDS`].
pub fn is_volatile_field(key: &str) -> bool {
    AGENT_VOLATILE_FIELDS.contains(&key)
}

pub(crate) fn columns(prefix: &str) -> String {
    AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("{prefix}\"{field}\""))
        .collect::<Vec<_>>()
        .join(",")
}

static TABLE_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "CREATE TABLE IF NOT EXISTS {AGENT_VOLATILE_TABLE}(id TEXT PRIMARY KEY,{});",
        AGENT_VOLATILE_FIELDS
            .iter()
            .map(|field| format!("\"{field}\" TEXT"))
            .collect::<Vec<_>>()
            .join(",")
    )
});

static SELECT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {} FROM {AGENT_VOLATILE_TABLE} WHERE id=?1",
        columns("")
    )
});

/// Upsert of the present fields: a NULL parameter keeps the stored column, and
/// an unchanged row is not written, so its triggers do not fire.
static UPSERT_SQL: LazyLock<String> = LazyLock::new(|| {
    let placeholders = (2..=AGENT_VOLATILE_FIELDS.len() + 1)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let next = |field: &str| format!("COALESCE(excluded.\"{field}\",\"{field}\")");
    let set = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("\"{field}\"={}", next(field)))
        .collect::<Vec<_>>()
        .join(",");
    let changed = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("{} IS NOT \"{field}\"", next(field)))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        "INSERT INTO {AGENT_VOLATILE_TABLE}(id,{}) VALUES(?1,{placeholders}) \
         ON CONFLICT(id) DO UPDATE SET {set} WHERE {changed}",
        columns("")
    )
});

/// The change journal and transcript revision statements that the
/// `runtime_agents` triggers run, for a volatile row of agent `NEW.id`.
const JOURNAL_BODY: &str = "UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; \
     UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; \
     INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; \
     UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1;";
const TRANSCRIPT_BODY: &str = "INSERT INTO runtime_transcript_revisions(agent,revision) VALUES (NEW.id,1) \
     ON CONFLICT(agent) DO UPDATE SET revision=revision+1;";

/// A trigger of this format: name, timing and event, table, and body.
pub(crate) struct Trigger {
    name: &'static str,
    event: &'static str,
    table: &'static str,
    body: &'static str,
}

/// Triggers that keep the volatile rows owned by their agent rows and journal
/// a volatile change like an agent record change.
const TRIGGERS: [Trigger; 3] = [
    Trigger {
        name: "runtime_agent_volatile_journal_insert",
        event: "AFTER INSERT",
        table: AGENT_VOLATILE_TABLE,
        body: JOURNAL_BODY,
    },
    Trigger {
        name: "runtime_agent_volatile_journal_update",
        event: "AFTER UPDATE",
        table: AGENT_VOLATILE_TABLE,
        body: JOURNAL_BODY,
    },
    OWNER_DELETE,
];

/// Deleting an agent row deletes its volatile row.
const OWNER_DELETE: Trigger = Trigger {
    name: "runtime_agent_volatile_owner_delete",
    event: "AFTER DELETE",
    table: "runtime_agents",
    body: "DELETE FROM runtime_agent_volatile WHERE id=OLD.id;",
};

/// Transcript revision triggers, present when `runtime_transcript_revisions`
/// exists: the transcript view shows `activity` and `contextUsage`.
const TRANSCRIPT_TRIGGERS: [Trigger; 2] = [
    Trigger {
        name: "runtime_agent_volatile_transcript_insert",
        event: "AFTER INSERT",
        table: AGENT_VOLATILE_TABLE,
        body: TRANSCRIPT_BODY,
    },
    Trigger {
        name: "runtime_agent_volatile_transcript_update",
        event: "AFTER UPDATE",
        table: AGENT_VOLATILE_TABLE,
        body: TRANSCRIPT_BODY,
    },
];

const TABLE_COLUMNS: [&str; AGENT_VOLATILE_FIELDS.len() + 1] = {
    let mut columns = ["id"; AGENT_VOLATILE_FIELDS.len() + 1];
    let mut index = 0;
    while index < AGENT_VOLATILE_FIELDS.len() {
        columns[index + 1] = AGENT_VOLATILE_FIELDS[index];
        index += 1;
    }
    columns
};

pub(crate) const TRIGGER_REQUIREMENTS: [TriggerRequirement; TRIGGERS.len()] = {
    let mut requirements = [TriggerRequirement {
        name: "",
        table: "",
    }; TRIGGERS.len()];
    let mut index = 0;
    while index < TRIGGERS.len() {
        requirements[index] = TriggerRequirement {
            name: TRIGGERS[index].name,
            table: TRIGGERS[index].table,
        };
        index += 1;
    }
    requirements
};

/// The table as the agents repository declares it; startup creates the table
/// and its triggers, so both are checked after it.
pub(crate) const TABLE_REQUIREMENT: TableRequirement = TableRequirement {
    name: AGENT_VOLATILE_TABLE,
    columns: &TABLE_COLUMNS,
};

/// The schema objects of this format, as (name, SQLite type, table); the
/// transcript triggers exist when `runtime_transcript_revisions` does.
pub fn schema_objects() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    std::iter::once((AGENT_VOLATILE_TABLE, "table", AGENT_VOLATILE_TABLE)).chain(
        TRIGGERS
            .iter()
            .chain(&TRANSCRIPT_TRIGGERS)
            .map(|trigger| (trigger.name, "trigger", trigger.table)),
    )
}

/// The table and its owner-delete trigger, for schema-only test fixtures
/// that already declare `runtime_agents`.
pub fn agent_volatile_table_sql() -> &'static str {
    static FIXTURE_SQL: LazyLock<String> = LazyLock::new(|| {
        format!(
            "{} CREATE TRIGGER IF NOT EXISTS {} {} ON {} BEGIN {} END;",
            *TABLE_SQL,
            OWNER_DELETE.name,
            OWNER_DELETE.event,
            OWNER_DELETE.table,
            OWNER_DELETE.body
        )
    });
    &FIXTURE_SQL
}

/// Create the table and its triggers. A state without the table gets it
/// through [`migrate_2026_10_20`], in the caller's startup transaction.
/// `RuntimeStore::open` calls it; so does a test that opens a Python-created
/// database directly.
pub fn prepare(transaction: &Transaction<'_>) -> StoreResult<()> {
    let created = !crate::schema::table_exists(transaction, AGENT_VOLATILE_TABLE)?;
    transaction.execute_batch(&TABLE_SQL)?;
    if created {
        // Before the triggers exist: the move changes no agent's content.
        migrate_2026_10_20(transaction)?;
    }
    let transcript = crate::schema::table_exists(transaction, "runtime_transcript_revisions")?;
    // A schema-only database without the change journal (a test fixture)
    // gets only the owner trigger; a state always has the journal.
    let journal = crate::schema::table_exists(transaction, "runtime_agent_record_generation")?;
    let triggers = TRIGGERS
        .iter()
        .filter(|trigger| journal || trigger.name == OWNER_DELETE.name)
        .chain(TRANSCRIPT_TRIGGERS.iter().filter(|_| transcript));
    for Trigger {
        name,
        event,
        table,
        body,
    } in triggers
    {
        transaction.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS {name} {event} ON {table} BEGIN {body} END;"
        ))?;
    }
    Ok(())
}

/// Tables that `runtime_agents` triggers advance on every record change. The
/// 2026-10-20 move changes no agent's content, so it restores them: no change
/// journal entry, transcript revision or sync projection follows from it.
const MIGRATION_PRESERVED_TABLES: [&str; 4] = [
    "runtime_agent_record_generation",
    "runtime_scheduler_agent_change_state",
    "runtime_scheduler_agent_changes",
    "runtime_transcript_revisions",
];

/// Data migration of 2026-10-20: move the volatile fields of every valid
/// agent record into [`AGENT_VOLATILE_TABLE`] and remove them from the record.
/// It runs once, in the transaction that creates the table, so a crash leaves
/// either the old format or the complete new one; a test oracle that writes
/// old-format records into a migrated state runs it again. Delete it once
/// every state has opened with a build of 2026-10-20 or later.
pub fn migrate_2026_10_20(transaction: &Transaction<'_>) -> StoreResult<()> {
    let present = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("json_type(record,'$.{field}') IS NOT NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let values = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("record->'$.{field}'"))
        .collect::<Vec<_>>()
        .join(",");
    let paths = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("'$.{field}'"))
        .collect::<Vec<_>>()
        .join(",");
    let moved = format!("json_valid(record) AND json_type(record)='object' AND ({present})");
    let mut preserved = Vec::new();
    for table in MIGRATION_PRESERVED_TABLES {
        if crate::schema::table_exists(transaction, table)? {
            transaction.execute_batch(&format!(
                "CREATE TEMP TABLE migration_2026_10_20_{table} AS SELECT * FROM main.{table};"
            ))?;
            preserved.push(table);
        }
    }
    // A field still in a record is newer than its volatile column: only a
    // writer without this format (a test oracle) puts it there.
    let newer = AGENT_VOLATILE_FIELDS
        .iter()
        .map(|field| format!("\"{field}\"=COALESCE(excluded.\"{field}\",\"{field}\")"))
        .collect::<Vec<_>>()
        .join(",");
    transaction.execute_batch(&format!(
        "INSERT INTO {AGENT_VOLATILE_TABLE}(id,{}) SELECT id,{values} FROM runtime_agents WHERE {moved}
         ON CONFLICT(id) DO UPDATE SET {newer};
         UPDATE runtime_agents SET record=json_remove(record,{paths}) WHERE {moved};",
        columns("")
    ))?;
    for table in preserved {
        transaction.execute_batch(&format!(
            "DELETE FROM main.{table};
             INSERT INTO main.{table} SELECT * FROM temp.migration_2026_10_20_{table};
             DROP TABLE temp.migration_2026_10_20_{table};"
        ))?;
    }
    Ok(())
}

/// The agents table whose records omit the volatile fields.
pub const AGENT_TABLE: &str = "runtime_agents";

/// Add agent `id`'s stored volatile fields to its decoded record object.
pub fn merge(connection: &Connection, id: &str, record: &mut Value) -> StoreResult<()> {
    #[cfg(any(test, feature = "test-support"))]
    publication_work::record(id, publication_work::Operation::VolatileLoad);
    let Some(fields) = record.as_object_mut() else {
        return Ok(());
    };
    let mut statement = connection.prepare_cached(&SELECT_SQL)?;
    let mut rows = statement.query([id])?;
    if let Some(row) = rows.next()? {
        merge_row(row, id, fields)?;
    }
    Ok(())
}

/// SQL for the JSON text of volatile `field` of the `runtime_agents` row that
/// `agents` (the table name or an alias) names, as [`merge`] reads it.
pub fn field_sql(agents: &str, field: &str) -> String {
    debug_assert!(
        is_volatile_field(field),
        "{field} is not a volatile agent field"
    );
    format!(
        "COALESCE((SELECT v.\"{field}\" FROM {AGENT_VOLATILE_TABLE} v WHERE v.id={agents}.id),\
         {agents}.record->'$.{field}')"
    )
}

/// Agent `id`'s stored record with its volatile fields, unchecked, for
/// diagnostics and test oracles that compare whole stored agents.
pub fn merged_record(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
    let raw: Option<String> = connection
        .query_row_cached(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| {
        let mut record = serde_json::from_str(&raw).map_err(|_| invalid(id))?;
        merge(connection, id, &mut record)?;
        Ok(record)
    })
    .transpose()
}

/// [`merge`] for whole-table dumps: each `{"id": .., "record": {..}}` row of
/// `runtime_agents` gets its volatile fields, for test oracles.
pub fn merge_dumped_rows(connection: &Connection, rows: &mut [Value]) -> StoreResult<()> {
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if let Some(record) = row.get_mut("record") {
            merge(connection, &id, record)?;
        }
    }
    Ok(())
}

/// [`merge`] for a decoded row of `table`, which may be any record table.
pub fn merge_if_agent(
    connection: &Connection,
    table: &str,
    id: &str,
    record: &mut Value,
) -> StoreResult<()> {
    if table == AGENT_TABLE {
        merge(connection, id, record)?;
    }
    Ok(())
}

/// The stored record text `raw` of agent `id` with its volatile fields
/// merged, for readers that pass record text on. Without a volatile row, or
/// for text that is not a JSON object, `raw` is returned unchanged.
pub fn merged_text(connection: &Connection, id: &str, raw: String) -> StoreResult<String> {
    #[cfg(any(test, feature = "test-support"))]
    publication_work::record(id, publication_work::Operation::VolatileLoad);
    let mut statement = connection.prepare_cached(&SELECT_SQL)?;
    let mut rows = statement.query([id])?;
    let Some(row) = rows.next()? else {
        return Ok(raw);
    };
    #[cfg(any(test, feature = "test-support"))]
    publication_work::record(id, publication_work::Operation::Decode);
    let Ok(Value::Object(mut fields)) = serde_json::from_str::<Value>(&raw) else {
        return Ok(raw);
    };
    merge_row(row, id, &mut fields)?;
    #[cfg(any(test, feature = "test-support"))]
    publication_work::record(id, publication_work::Operation::TextRoundTrip);
    Ok(serde_json::to_string(&fields)?)
}

/// [`merged_text`] for `(id, record)` agent rows.
pub fn merged_rows(
    connection: &Connection,
    rows: Vec<(String, String)>,
) -> StoreResult<Vec<(String, String)>> {
    rows.into_iter()
        .map(|(id, raw)| {
            let raw = merged_text(connection, &id, raw)?;
            Ok((id, raw))
        })
        .collect()
}

pub(crate) fn merge_row(
    row: &rusqlite::Row<'_>,
    id: &str,
    fields: &mut Map<String, Value>,
) -> StoreResult<()> {
    for (index, field) in AGENT_VOLATILE_FIELDS.iter().enumerate() {
        match row.get_ref(index)? {
            ValueRef::Null => {}
            ValueRef::Text(text) => {
                let value = serde_json::from_slice(text).map_err(|_| invalid(id))?;
                fields.insert((*field).to_owned(), value);
            }
            _ => return Err(invalid(id)),
        }
    }
    Ok(())
}

fn invalid(id: &str) -> StoreError {
    StoreError::InvalidStoredObject {
        table: AGENT_VOLATILE_TABLE,
        id: id.to_owned(),
    }
}

/// The record object without the volatile fields, serialized without a copy.
struct StoredRecord<'a>(&'a Map<String, Value>);

impl Serialize for StoredRecord<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in self.0 {
            if !is_volatile_field(key) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

/// The `runtime_agents.record` text of a checked agent object.
fn stored_record(agent: &Map<String, Value>) -> StoreResult<String> {
    Ok(serde_json::to_string(&StoredRecord(agent))?)
}

/// Store the volatile fields present in `agent` for agent `id`.
fn put_fields(connection: &Connection, id: &str, agent: &Map<String, Value>) -> StoreResult<()> {
    let mut values: [Option<String>; AGENT_VOLATILE_FIELDS.len()] = Default::default();
    let mut present = false;
    for (slot, field) in values.iter_mut().zip(AGENT_VOLATILE_FIELDS) {
        if let Some(value) = agent.get(field) {
            *slot = Some(serde_json::to_string(value)?);
            present = true;
        }
    }
    if present {
        connection.execute_cached(
            &UPSERT_SQL,
            params_from_iter(std::iter::once(Some(id.to_owned())).chain(values)),
        )?;
    }
    Ok(())
}

fn object<'a>(id: &str, agent: &'a Value) -> StoreResult<&'a Map<String, Value>> {
    agent
        .as_object()
        .filter(|fields| fields.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| StoreError::InvalidStoredObject {
            table: AGENT_TABLE,
            id: id.to_owned(),
        })
}

/// Insert or replace agent `id`, a checked agent object. With `insert_only`,
/// an existing row is kept. Returns whether a row was inserted or its record
/// replaced; an unchanged record is not rewritten and returns false.
pub fn put(
    connection: &Connection,
    id: &str,
    agent: &Value,
    insert_only: bool,
) -> StoreResult<bool> {
    let agent = object(id, agent)?;
    let record = stored_record(agent)?;
    let sql = if insert_only {
        "INSERT INTO runtime_agents(id,record) VALUES (?1,?2) ON CONFLICT(id) DO NOTHING"
    } else {
        "INSERT INTO runtime_agents(id,record) VALUES (?1,?2) ON CONFLICT(id) \
         DO UPDATE SET record=excluded.record WHERE record IS NOT excluded.record"
    };
    let written = connection.execute_cached(sql, params![id, record])? == 1;
    if written || !insert_only {
        put_fields(connection, id, agent)?;
    }
    Ok(written)
}

/// Replace agent `id`, a checked agent object, when its row exists; returns
/// whether it exists. An unchanged record is not rewritten.
pub fn update(connection: &Connection, id: &str, agent: &Value) -> StoreResult<bool> {
    let agent = object(id, agent)?;
    let record = stored_record(agent)?;
    let written = connection.execute_cached(
        "UPDATE runtime_agents SET record=?2 WHERE id=?1 AND record IS NOT ?2",
        params![id, record],
    )? == 1;
    let exists = written
        || connection
            .query_row_cached("SELECT 1 FROM runtime_agents WHERE id=?1", [id], |_| Ok(()))
            .optional()?
            .is_some();
    if exists {
        put_fields(connection, id, agent)?;
    }
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentId, AgentRepository, RuntimeStore, system_reads};
    use serde_json::json;

    fn agents() -> Vec<Value> {
        vec![
            json!({"id":"a","rootId":"a","epoch":1,"status":"running","threadId":"t",
                "events":7,"lastEvent":"2026-10-20T00:00:00Z","activity":{"phase":"writing","at":5.0},
                "activeTools":[],"tail":"answer tail","lastAnswer":"answer","contextUsage":null,
                "tokensUsed":120,"tokenUsageAccounting":"total","prompt":"keep"}),
            json!({"id":"b","rootId":"a","epoch":0,"status":"idle","name":"no volatile fields"}),
        ]
    }

    fn stored(connection: &Connection, id: &str) -> (Value, Option<Vec<Option<String>>>) {
        let record: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .expect("agent row");
        let volatile = connection
            .query_row(&SELECT_SQL, [id], |row| {
                (0..AGENT_VOLATILE_FIELDS.len())
                    .map(|index| row.get::<_, Option<String>>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .optional()
            .expect("volatile row reads");
        (
            serde_json::from_str(&record).expect("record JSON"),
            volatile,
        )
    }

    /// A pre-2026-10-20 state migrates once at open; every reader sees the
    /// agents unchanged, and a second open changes nothing.
    #[tokio::test]
    async fn migration_2026_10_20_moves_fields_once_and_readers_see_the_same_agents() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        store
            .transaction(|tx| {
                for object in schema_objects().filter(|(_, kind, _)| *kind == "trigger") {
                    tx.execute_batch(&format!("DROP TRIGGER IF EXISTS {}", object.0))?;
                }
                tx.execute_batch(&format!("DROP TABLE {AGENT_VOLATILE_TABLE}"))?;
                for agent in agents() {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        params![agent["id"].as_str(), agent.to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("old-format state");
        let journal = |connection: &Connection| -> Vec<String> {
            MIGRATION_PRESERVED_TABLES
                .iter()
                .flat_map(|table| {
                    let mut statement = connection
                        .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                        .expect("journal table");
                    let count = statement.column_count();
                    statement
                        .query_map([], |row| {
                            (0..count)
                                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                                .collect::<rusqlite::Result<Vec<_>>>()
                        })
                        .expect("journal rows")
                        .map(|row| format!("{table}: {:?}", row.expect("journal row")))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let journal_before = journal(&store.test_reader().expect("reader"));
        drop(store);

        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store migrates");
        let connection = store.test_reader().expect("reader");
        assert_eq!(
            journal(&connection),
            journal_before,
            "the move journals no change"
        );
        let migrated = agents()
            .iter()
            .map(|agent| stored(&connection, agent["id"].as_str().expect("id")))
            .collect::<Vec<_>>();
        for (agent, (record, volatile)) in agents().iter().zip(&migrated) {
            let id = agent["id"].as_str().expect("id");
            assert!(
                AGENT_VOLATILE_FIELDS
                    .iter()
                    .all(|field| record.get(field).is_none())
            );
            let has_fields = AGENT_VOLATILE_FIELDS
                .iter()
                .any(|field| agent.get(field).is_some());
            assert_eq!(volatile.is_some(), has_fields, "{id} volatile row");
            let typed = AgentRepository::get(&connection, &AgentId::from(id))
                .expect("typed read")
                .expect("agent exists");
            assert_eq!(&typed.value(), agent);
            assert_eq!(
                system_reads::get(&connection, "runtime_agents", id).expect("checked read"),
                Some(agent.clone())
            );
        }
        drop(connection);
        drop(store);
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store reopens");
        let connection = store.test_reader().expect("reader");
        for (agent, before) in agents().iter().zip(&migrated) {
            assert_eq!(
                &stored(&connection, agent["id"].as_str().expect("id")),
                before
            );
        }
    }

    /// A write that changes only volatile fields leaves `runtime_agents`
    /// untouched; an absent field keeps its value; an unchanged write writes
    /// nothing.
    #[tokio::test]
    async fn volatile_only_writes_leave_the_indexed_record_alone() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        store
            .transaction(|tx| {
                let mut agent = agents()[0].clone();
                put(tx, "a", &agent, false)?;
                tx.execute_batch(
                    "CREATE TEMP TABLE probe(n INTEGER); INSERT INTO probe VALUES(0);
                     CREATE TEMP TRIGGER probe_agents AFTER UPDATE ON main.runtime_agents
                     BEGIN UPDATE probe SET n=n+1; END;",
                )?;
                let revision = |tx: &Transaction<'_>| -> rusqlite::Result<i64> {
                    tx.query_row(
                        "SELECT revision FROM runtime_transcript_revisions WHERE agent='a'",
                        [],
                        |row| row.get(0),
                    )
                };
                let revision_before = revision(tx)?;
                agent["events"] = json!(8);
                agent["tail"] = json!("longer answer tail");
                assert!(update(tx, "a", &agent)?);
                let agent_updates: i64 =
                    tx.query_row("SELECT n FROM probe", [], |row| row.get(0))?;
                assert_eq!(agent_updates, 0, "runtime_agents is not written");
                assert_eq!(
                    revision(tx)?,
                    revision_before + 1,
                    "the transcript revision advances"
                );
                let mut partial = agent.clone();
                let object = partial.as_object_mut().expect("object");
                object.remove("lastAnswer");
                let before = tx.total_changes();
                assert!(update(tx, "a", &partial)?);
                assert_eq!(
                    tx.total_changes(),
                    before,
                    "an unchanged write writes nothing"
                );
                let mut read = Value::Object(stored_record_object(tx, "a"));
                merge(tx, "a", &mut read)?;
                assert_eq!(read, agent);
                Ok(())
            })
            .await
            .expect("writes");
    }

    fn stored_record_object(connection: &Connection, id: &str) -> Map<String, Value> {
        match stored(connection, id).0 {
            Value::Object(fields) => fields,
            _ => panic!("record is an object"),
        }
    }
}

/// Writer-local witnesses for full-agent publication work. Active only within
/// the final publication scope, excluding source mutations and fixture reads.
#[cfg(any(test, feature = "test-support"))]
pub mod publication_work {
    use std::{cell::RefCell, collections::BTreeMap};

    #[derive(Clone, Copy)]
    pub enum Operation {
        Decode,
        VolatileLoad,
        TextRoundTrip,
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    pub struct Calls {
        pub decodes: usize,
        pub volatile_loads: usize,
        pub text_round_trips: usize,
    }

    #[derive(Default)]
    struct State {
        active: bool,
        calls: BTreeMap<String, Calls>,
    }
    thread_local! {
        static STATE: RefCell<State> = RefCell::new(State::default());
    }
    pub struct Scope;
    pub fn scope() -> Scope {
        STATE.with_borrow_mut(|state| state.active = true);
        Scope
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            STATE.with_borrow_mut(|state| state.active = false);
        }
    }
    pub fn take() -> BTreeMap<String, Calls> {
        STATE.with_borrow_mut(|state| std::mem::take(&mut state.calls))
    }
    pub fn record(id: &str, operation: Operation) {
        STATE.with_borrow_mut(|state| {
            if !state.active {
                return;
            }
            let calls = state.calls.entry(id.to_owned()).or_default();
            match operation {
                Operation::Decode => calls.decodes += 1,
                Operation::VolatileLoad => calls.volatile_loads += 1,
                Operation::TextRoundTrip => calls.text_round_trips += 1,
            }
        });
    }
}
