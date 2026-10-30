//! Read workspace upload records written by the Python runtime.

use crate::records::{Table, TranscriptAssetId, TranscriptAssetRecord};
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, params};

/// Compatibility name for callers that used the pre-typed asset wrapper.
pub type StoredWorkspaceAsset = TranscriptAssetRecord;

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_agents",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_assets",
        columns: &["id", "record"],
    },
];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TABLES[0]],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TABLES[1]],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceFilesRepository;

impl WorkspaceFilesRepository {
    pub fn asset(connection: &Connection, id: &str) -> StoreResult<Option<TranscriptAssetRecord>> {
        Table::<TranscriptAssetRecord>::new(connection).get(&TranscriptAssetId::from(id))
    }

    pub fn asset_ids(connection: &Connection) -> StoreResult<Vec<String>> {
        let mut statement = connection.prepare_cached("SELECT id FROM runtime_assets")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn insert_asset(
        connection: &Connection,
        id: &str,
        record: &TranscriptAssetRecord,
    ) -> StoreResult<()> {
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: "runtime_assets",
                id: id.to_owned(),
            });
        }
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
    fn asset_insert_rejects_invalid_shape_and_row_identity() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_assets(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let record = serde_json::json!({"id":"asset","agent":"a","name":"file","path":root.path().join("file"),"mime":"text/plain","image":false,"size":1.0,"hash":"h","created":0.0,"future":{"keep":true}});
        let typed = crate::TranscriptAssetRecord::from_value(record.clone()).unwrap();
        assert!(WorkspaceFilesRepository::insert_asset(&db, "other", &typed).is_err());
        let mut invalid = record.clone();
        invalid["image"] = serde_json::json!("bad");
        assert!(crate::TranscriptAssetRecord::from_value(invalid).is_err());
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_assets", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        WorkspaceFilesRepository::insert_asset(&db, "asset", &typed).unwrap();
        assert_eq!(
            WorkspaceFilesRepository::asset(&db, "asset")
                .unwrap()
                .unwrap()
                .into_value(),
            record
        );
    }

    #[test]
    fn reads_asset_record_by_id() {
        let db = Connection::open_in_memory().expect("in-memory database opens");
        db.execute_batch("CREATE TABLE runtime_assets(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .expect("asset table creates");
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.txt");
        db.execute("INSERT INTO runtime_assets VALUES('asset',?1)", [serde_json::json!({"id":"asset","agent":"agent","name":"a.txt","path":path,"mime":"text/plain","image":false,"size":1,"hash":"h","created":0}).to_string()]).unwrap();

        let asset = WorkspaceFilesRepository::asset(&db, "asset")
            .expect("asset query works")
            .expect("asset exists");
        assert_eq!(asset.id().as_ref(), "asset");
        assert_eq!(
            asset.into_value(),
            serde_json::json!({
                "id": "asset",
                "agent": "agent",
                "name": "a.txt",
                "path": path,
                "mime": "text/plain",
                "image": false,
                "size": 1,
                "hash": "h",
                "created": 0,
            })
        );
        assert!(
            WorkspaceFilesRepository::asset(&db, "missing")
                .expect("missing asset query works")
                .is_none()
        );
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "workspace_files",
        requirements: &REQUIREMENTS,
    }
}
