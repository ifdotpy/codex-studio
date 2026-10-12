//! Read the persisted work-item, plan and queue state without creating tables.

use rusqlite::{Connection, OptionalExtension, params, types::ValueRef};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

macro_rules! optional_read {
    ($table:literal) => {
        ConditionalTableRequirement {
            table_exists: $table,
            tables: &[TableRequirement {
                name: $table,
                columns: &["id", "record"],
            }],
        }
    };
}

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        optional_read!("runtime_work"),
        optional_read!("runtime_plans"),
        optional_read!("runtime_annotations"),
        optional_read!("runtime_event_meta"),
        optional_read!("runtime_assets"),
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
    pub record: Option<String>,
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
    pub valid: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkItemsRepository;

impl WorkItemsRepository {
    pub fn work(connection: &Connection) -> StoreResult<Vec<StoredRecord>> {
        if !table_exists(connection, "runtime_work")? {
            return Ok(Vec::new());
        }
        let mut statement =
            connection.prepare("SELECT id, record FROM runtime_work ORDER BY rowid")?;
        let rows = statement.query_map([], |row| {
            Ok(StoredRecord {
                id: row.get(0)?,
                record: text_column(row.get_ref(1)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn plan(connection: &Connection, id: &str) -> StoreResult<Option<StoredRecord>> {
        if !table_exists(connection, "runtime_plans")? {
            return Ok(None);
        }
        Ok(connection
            .query_row(
                "SELECT id, record FROM runtime_plans WHERE id = ?1",
                [id],
                |row| {
                    Ok(StoredRecord {
                        id: row.get(0)?,
                        record: text_column(row.get_ref(1)?),
                    })
                },
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
            let (id, id_valid) = row.get::<_, String>(0).map_or_else(
                |_| ("<invalid id>".to_owned(), false),
                |value| (value, true),
            );
            let (kind, kind_valid) = row
                .get::<_, String>(1)
                .map_or_else(|_| (String::new(), false), |value| (value, true));
            let (text, text_valid) = row
                .get::<_, String>(2)
                .map_or_else(|_| (String::new(), false), |value| (value, true));
            let (status, status_valid) = row
                .get::<_, String>(3)
                .map_or_else(|_| (String::new(), false), |value| (value, true));
            let (created, created_valid) = row
                .get::<_, f64>(4)
                .map_or_else(|_| (f64::NAN, false), |value| (value, value.is_finite()));
            let (error, error_valid) = row.get_ref(5).map(optional_text).unwrap_or((None, false));
            let (metadata, metadata_valid) =
                row.get_ref(6).map(optional_text).unwrap_or((None, false));
            Ok(StoredQueueEvent {
                id,
                kind,
                text,
                status,
                created,
                error,
                metadata,
                valid: id_valid
                    && kind_valid
                    && text_valid
                    && status_valid
                    && created_valid
                    && error_valid
                    && metadata_valid,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn assets(
        connection: &Connection,
        ids: &[String],
    ) -> StoreResult<Option<Vec<StoredRecord>>> {
        if !table_exists(connection, "runtime_assets")? {
            return Ok(None);
        }
        if ids.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let mut statement = connection.prepare("SELECT record FROM runtime_assets WHERE id=?1")?;
        let mut result = Vec::with_capacity(ids.len());
        let mut seen = std::collections::BTreeSet::new();
        for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
            if let Some(record) = statement
                .query_row([id], |row| Ok(text_column(row.get_ref(0)?)))
                .optional()?
            {
                result.push(StoredRecord {
                    id: id.clone(),
                    record,
                });
            }
        }
        Ok(Some(result))
    }
}

fn text_column(value: ValueRef<'_>) -> Option<String> {
    value.as_str().ok().map(str::to_owned)
}

fn optional_text(value: ValueRef<'_>) -> (Option<String>, bool) {
    match value {
        ValueRef::Null => (None, true),
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => (Some(text.to_owned()), true),
            Err(_) => (None, false),
        },
        _ => (None, false),
    }
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
