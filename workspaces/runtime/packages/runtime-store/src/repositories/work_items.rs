//! Read the persisted work-item, plan and queue state without creating tables.

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const WORK_TABLE: TableRequirement = TableRequirement {
    name: "runtime_work",
    columns: &["id", "record"],
};
const PLAN_TABLE: TableRequirement = TableRequirement {
    name: "runtime_plans",
    columns: &["id", "record"],
};
const ANNOTATION_TABLE: TableRequirement = TableRequirement {
    name: "runtime_annotations",
    columns: &["id", "record"],
};
const EVENT_META_TABLE: TableRequirement = TableRequirement {
    name: "runtime_event_meta",
    columns: &["id", "record"],
};
const ASSET_TABLE: TableRequirement = TableRequirement {
    name: "runtime_assets",
    columns: &["id", "record"],
};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_work",
            tables: &[WORK_TABLE],
        },
        ConditionalTableRequirement {
            table_exists: "runtime_plans",
            tables: &[PLAN_TABLE],
        },
        ConditionalTableRequirement {
            table_exists: "runtime_annotations",
            tables: &[ANNOTATION_TABLE],
        },
        ConditionalTableRequirement {
            table_exists: "runtime_event_meta",
            tables: &[EVENT_META_TABLE],
        },
        ConditionalTableRequirement {
            table_exists: "runtime_assets",
            tables: &[ASSET_TABLE],
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct StoredRecord {
    pub id: String,
    pub record: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredQueueEvent {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub status: String,
    pub created: f64,
    pub error: Option<String>,
    pub metadata: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkItemsRepository;

impl WorkItemsRepository {
    pub fn work(connection: &Connection) -> StoreResult<Vec<StoredRecord>> {
        records_if_present(connection, "runtime_work")
    }

    pub fn plan(connection: &Connection, id: &str) -> StoreResult<Option<String>> {
        if !table_exists(connection, "runtime_plans")? {
            return Ok(None);
        }
        Ok(connection
            .query_row(
                "SELECT record FROM runtime_plans WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn queue(
        connection: &Connection,
        agent: &str,
        epoch: i64,
    ) -> StoreResult<Vec<StoredQueueEvent>> {
        let sql = if table_exists(connection, "runtime_event_meta")? {
            "SELECT e.id, e.kind, e.text, e.status, e.created, e.error, m.record \
             FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id \
             WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
             AND e.kind IN ('user','followup') ORDER BY e.created, e.rowid"
        } else {
            "SELECT e.id, e.kind, e.text, e.status, e.created, e.error, NULL \
             FROM runtime_events e WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
             AND e.kind IN ('user','followup') ORDER BY e.created, e.rowid"
        };
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map(params![agent, epoch], |row| {
            Ok(StoredQueueEvent {
                id: row.get(0)?,
                kind: row.get(1)?,
                text: row.get(2)?,
                status: row.get(3)?,
                created: row.get(4)?,
                error: row.get(5)?,
                metadata: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn assets(connection: &Connection, ids: &[String]) -> StoreResult<Vec<StoredRecord>> {
        if ids.is_empty() || !table_exists(connection, "runtime_assets")? {
            return Ok(Vec::new());
        }
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(record) = connection
                .query_row(
                    "SELECT record FROM runtime_assets WHERE id=?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                result.push(StoredRecord {
                    id: id.clone(),
                    record,
                });
            }
        }
        Ok(result)
    }
}

fn records_if_present(connection: &Connection, table: &str) -> StoreResult<Vec<StoredRecord>> {
    if !table_exists(connection, table)? {
        return Ok(Vec::new());
    }
    let mut statement =
        connection.prepare(&format!("SELECT id, record FROM {table} ORDER BY rowid"))?;
    let rows = statement.query_map([], |row| {
        Ok(StoredRecord {
            id: row.get(0)?,
            record: row.get(1)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE)",
        [table],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::WorkItemsRepository;
    use rusqlite::Connection;

    #[test]
    fn absent_work_tables_are_an_empty_read() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        assert!(
            WorkItemsRepository::work(&connection)
                .expect("missing work table is optional")
                .is_empty()
        );
        assert_eq!(
            WorkItemsRepository::plan(&connection, "agent-a")
                .expect("missing plan table is optional"),
            None
        );
    }
}
