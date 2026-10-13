//! Persisted project metadata and its renderer sync projection.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult, SyncDocumentsRepository};

const PROJECT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_projects",
    columns: &["id", "record"],
}];
const SIDEBAR_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_sidebar_order",
    columns: &["id", "record"],
}];
const RECEIPT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_operation_receipts",
    columns: &["id", "signature", "result"],
}];
const AGENT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_agents",
    columns: &["id", "record"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_projects",
            tables: PROJECT_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_sidebar_order",
            tables: SIDEBAR_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_operation_receipts",
            tables: RECEIPT_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_agents",
            tables: AGENT_TABLE,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct ProjectsRepository;

impl ProjectsRepository {
    pub fn list(connection: &Connection) -> StoreResult<Vec<(String, Value)>> {
        let mut statement = connection.prepare(
            "SELECT id,record FROM runtime_projects ORDER BY json_extract(record,'$.created'),id",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, raw) = row?;
                let value = serde_json::from_str(&raw)?;
                Ok((id, value))
            })
            .collect()
    }

    pub fn get(connection: &Connection, id: &str) -> StoreResult<Option<(String, Value)>> {
        let raw = connection
            .query_row(
                "SELECT record FROM runtime_projects WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        raw.map(|raw| Ok((raw.clone(), serde_json::from_str(&raw)?)))
            .transpose()
    }

    pub fn receipt(connection: &Connection, id: &str) -> StoreResult<Option<(String, Value)>> {
        let row = connection
            .query_row(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(signature, raw)| Ok((signature, serde_json::from_str(&raw)?)))
            .transpose()
    }

    pub fn save_receipt(
        transaction: &Transaction<'_>,
        id: &str,
        signature: &str,
        result: &Value,
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3)",
            params![id, signature, serde_json::to_string(result)?],
        )?;
        Ok(())
    }

    pub fn agents(connection: &Connection) -> StoreResult<Vec<(String, Value)>> {
        let mut statement = connection.prepare("SELECT id,record FROM runtime_agents")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, raw) = row?;
                Ok((id, serde_json::from_str(&raw)?))
            })
            .collect()
    }

    pub fn put_project(transaction: &Transaction<'_>, project: &Value) -> StoreResult<()> {
        let id = project
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| StoreError::SchemaMismatch("project id is invalid".to_owned()))?;
        let record = serde_json::to_string(project)?;
        transaction.execute(
            "INSERT INTO runtime_projects(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, record],
        )?;
        SyncDocumentsRepository::put_entity(transaction, "project", id, project.clone(), false)?;
        Ok(())
    }

    pub fn remove(transaction: &Transaction<'_>, id: &str) -> StoreResult<bool> {
        let removed = transaction.execute("DELETE FROM runtime_projects WHERE id=?1", [id])? > 0;
        if removed {
            SyncDocumentsRepository::put_entity(transaction, "project", id, json!({}), true)?;
        }
        Ok(removed)
    }

    pub fn sidebar_order(connection: &Connection) -> StoreResult<Value> {
        connection
            .query_row(
                "SELECT record FROM runtime_sidebar_order WHERE id='current'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|raw| serde_json::from_str(&raw).map_err(StoreError::from))
            .transpose()
            .map(|value| value.unwrap_or_else(|| json!({"revision": 0, "groups": null})))
    }

    pub fn put_sidebar_order(transaction: &Transaction<'_>, value: &Value) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO runtime_sidebar_order(id,record) VALUES ('current',?1) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            [serde_json::to_string(value)?],
        )?;
        let envelope = transaction
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let mut envelope: Value = match envelope {
            Some(raw) => serde_json::from_str(&raw).map_err(|_| StoreError::InvalidSyncObject {
                scope: "workspace".to_owned(),
                id: "current".to_owned(),
            })?,
            None => json!({"collection":"workspace","id":"current","value":{}}),
        };
        if envelope.get("collection").and_then(Value::as_str) != Some("workspace")
            || envelope.get("id").and_then(Value::as_str) != Some("current")
        {
            return Err(StoreError::InvalidSyncObject {
                scope: "workspace".to_owned(),
                id: "current".to_owned(),
            });
        }
        let projection = envelope
            .get_mut("value")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| StoreError::InvalidSyncObject {
                scope: "workspace".to_owned(),
                id: "current".to_owned(),
            })?;
        projection.insert("sidebarOrder".to_owned(), value.clone());
        SyncDocumentsRepository::put_entity(
            transaction,
            "workspace",
            "current",
            envelope["value"].clone(),
            false,
        )?;
        Ok(())
    }
}
