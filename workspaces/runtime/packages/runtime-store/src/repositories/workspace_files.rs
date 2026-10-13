//! Read workspace upload records written by the Python runtime.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TableRequirement {
        name: "runtime_agents",
        columns: &["id", "record"],
    }],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TableRequirement {
        name: "runtime_assets",
        columns: &["id", "record"],
    }],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredWorkspaceAsset {
    pub id: String,
    pub record: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceFilesRepository;

impl WorkspaceFilesRepository {
    pub fn asset(connection: &Connection, id: &str) -> StoreResult<Option<StoredWorkspaceAsset>> {
        connection
            .query_row(
                "SELECT id,record FROM runtime_assets WHERE id=?1",
                [id],
                |row| {
                    Ok(StoredWorkspaceAsset {
                        id: row.get(0)?,
                        record: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn asset_ids(connection: &Connection) -> StoreResult<Vec<String>> {
        let mut statement = connection.prepare("SELECT id FROM runtime_assets")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn insert_asset(connection: &Connection, id: &str, record: &Value) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_assets(id,record) VALUES (?1,?2)",
            params![id, serde_json::to_string(record)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::WorkspaceFilesRepository;

    #[test]
    fn reads_asset_record_by_id() {
        let db = Connection::open_in_memory().expect("in-memory database opens");
        db.execute_batch(
            "CREATE TABLE runtime_assets(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_assets VALUES('asset','{\"id\":\"asset\"}');",
        )
        .expect("asset table creates");

        let asset = WorkspaceFilesRepository::asset(&db, "asset")
            .expect("asset query works")
            .expect("asset exists");
        assert_eq!(asset.id, "asset");
        assert_eq!(asset.record, "{\"id\":\"asset\"}");
        assert!(
            WorkspaceFilesRepository::asset(&db, "missing")
                .expect("missing asset query works")
                .is_none()
        );
    }
}
