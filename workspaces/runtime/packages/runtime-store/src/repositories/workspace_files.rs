//! Read workspace upload records written by the Python runtime.

use rusqlite::Connection;

use crate::StoreResult;
use crate::records::{Table, TranscriptAssetId, TranscriptAssetRecord};
use crate::schema::{SchemaRequirements, TableRequirement};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[
        TableRequirement {
            name: "runtime_agents",
            columns: &["id", "record"],
        },
        TableRequirement {
            name: "runtime_assets",
            columns: &["id", "record"],
        },
    ],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceFilesRepository;

impl WorkspaceFilesRepository {
    pub fn asset(connection: &Connection, id: &str) -> StoreResult<Option<TranscriptAssetRecord>> {
        Table::<TranscriptAssetRecord>::new(connection).get(&TranscriptAssetId::from(id))
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
             INSERT INTO runtime_assets VALUES('asset','{\"id\":\"asset\",\"agent\":\"agent\",\"name\":\"a.txt\",\"path\":\"/tmp/a.txt\",\"mime\":\"text/plain\",\"image\":false,\"size\":1,\"hash\":\"h\",\"created\":0}');",
        )
        .expect("asset table creates");

        let asset = WorkspaceFilesRepository::asset(&db, "asset")
            .expect("asset query works")
            .expect("asset exists");
        assert_eq!(asset.id().as_ref(), "asset");
        assert_eq!(asset.value(), &serde_json::json!({"id":"asset"}));
        assert!(
            WorkspaceFilesRepository::asset(&db, "missing")
                .expect("missing asset query works")
                .is_none()
        );
    }
}
