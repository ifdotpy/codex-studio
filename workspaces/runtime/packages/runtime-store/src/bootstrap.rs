use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::{StoreError, StoreResult};

const SYNC_SCHEMA: &str = include_str!("sync_schema.sql");

struct ParentTableRegistration(&'static str);

inventory::collect!(ParentTableRegistration);

inventory::submit! {
    ParentTableRegistration("runtime_agents")
}

inventory::submit! {
    ParentTableRegistration("runtime_events")
}

inventory::submit! {
    ParentTableRegistration("runtime_items")
}

fn registered_parent_tables() -> Vec<&'static str> {
    let mut tables = inventory::iter::<ParentTableRegistration>
        .into_iter()
        .map(|registration| registration.0)
        .collect::<Vec<_>>();
    tables.sort_unstable();
    tables
}

pub(crate) fn prepare_sync_schema(transaction: &Transaction<'_>) -> StoreResult<()> {
    let sections = parse_sections(SYNC_SCHEMA)?;
    let has_runtime_items = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_items' COLLATE NOCASE)",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    let (expected, findings) = expected_catalog(transaction, sections, has_runtime_items)?;
    if let Some(finding) = findings.into_iter().next() {
        return Err(schema_object_mismatch(&finding.object, &finding.reason));
    }

    reconcile_derived_objects(transaction, &expected)?;
    drop_legacy_watch_triggers(transaction)?;
    transaction.execute_batch(sections[0])?;
    transaction.execute_batch(sections[1])?;
    transaction.execute_batch(sections[2])?;
    if has_runtime_items {
        transaction.execute_batch(sections[3])?;
    }
    Ok(())
}

fn parse_sections(source: &str) -> StoreResult<[&str; 4]> {
    let mut sections = source.split("-- section: ").skip(1);
    let mut bodies = [""; 4];
    for (body, expected) in bodies.iter_mut().zip([
        "common",
        "runtime_events",
        "runtime_agents",
        "runtime_items",
    ]) {
        let section = sections.next().ok_or_else(|| {
            StoreError::SchemaMismatch(format!("sync schema is missing {expected}"))
        })?;
        let (name, sql) = section.split_once('\n').ok_or_else(|| {
            StoreError::SchemaMismatch(format!("sync schema section {expected} has no body"))
        })?;
        if name.trim() != expected {
            return Err(StoreError::SchemaMismatch(format!(
                "expected sync schema section {expected}, found {name}"
            )));
        }
        *body = sql;
    }
    if sections.next().is_some() {
        return Err(StoreError::SchemaMismatch(
            "unexpected sync schema section".into(),
        ));
    }
    Ok(bodies)
}

fn drop_legacy_watch_triggers(transaction: &Transaction<'_>) -> StoreResult<()> {
    let mut statement = transaction.prepare(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name GLOB 'sync_watch_*'",
    )?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for name in names {
        transaction.execute_batch(&format!("DROP TRIGGER {}", quote_identifier(&name)))?;
    }
    Ok(())
}

type ColumnLayout = (String, Option<String>, i64, Option<String>, i64, i64);
type ConstraintIndex = (String, Vec<(i64, i64, Option<String>, i64, Option<String>)>);
type TableLayout = (Vec<ColumnLayout>, bool, bool, BTreeSet<ConstraintIndex>);
type CatalogObject = (String, String, String);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SchemaFinding {
    pub(crate) object: String,
    pub(crate) reason: String,
}

/// Inspect sync table signatures using a read-only connection.
///
/// This builds the expected tables from the canonical DDL in an in-memory
/// database and returns every table difference without writing to `connection`.
#[allow(dead_code)] // Used by the pre-cutover state check when that checker is added.
pub(crate) fn inspect_sync_tables(connection: &Connection) -> StoreResult<Vec<SchemaFinding>> {
    let sections = parse_sections(SYNC_SCHEMA)?;
    let has_runtime_items = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_items' COLLATE NOCASE)",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    let (_, findings) = expected_catalog(connection, sections, has_runtime_items)?;
    Ok(findings)
}

fn expected_catalog(
    connection: &Connection,
    sections: [&str; 4],
    has_runtime_items: bool,
) -> StoreResult<(Vec<CatalogObject>, Vec<SchemaFinding>)> {
    let expected = Connection::open_in_memory()?;
    let parent_tables = registered_parent_tables();
    for parent in &parent_tables {
        if *parent == "runtime_items" && !has_runtime_items {
            continue;
        }
        let sql = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE",
                [*parent],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| schema_object_mismatch(parent, "required parent table is missing"))?;
        expected
            .execute_batch(&sql)
            .map_err(|error| schema_object_mismatch(parent, &error.to_string()))?;
    }
    expected.execute_batch(sections[0])?;
    expected.execute_batch(sections[1])?;
    expected.execute_batch(sections[2])?;
    if has_runtime_items {
        expected.execute_batch(sections[3])?;
    }

    let mut statement = expected.prepare(
        "SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        ))
    })?;
    let mut derived = Vec::new();
    let mut findings = Vec::new();
    for object in rows {
        let (kind, name, sql) = object?;
        if kind == "table" {
            if parent_tables
                .iter()
                .any(|parent| parent.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            let wanted = table_layout(&expected, &name)?
                .ok_or_else(|| schema_object_mismatch(&name, "scratch table is missing"))?;
            let actual_kind = connection
                .query_row(
                    "SELECT type FROM sqlite_master WHERE name=?1 COLLATE NOCASE AND type IN ('table','view','index')",
                    [&name],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(actual_kind) = actual_kind {
                if actual_kind != "table" {
                    findings.push(SchemaFinding {
                        object: name,
                        reason: format!("expected a table, found {actual_kind}"),
                    });
                    continue;
                }
                let actual = table_layout(connection, &name)?;
                if actual.as_ref() == Some(&wanted) {
                    continue;
                }
                let actual = actual.as_ref().expect("existing table has a layout");
                let column = wanted
                    .0
                    .iter()
                    .zip(&actual.0)
                    .find(|(expected, actual)| expected != actual)
                    .map(|(expected, _)| expected.0.as_str())
                    .or_else(|| wanted.0.get(actual.0.len()).map(|column| column.0.as_str()))
                    .or_else(|| actual.0.get(wanted.0.len()).map(|column| column.0.as_str()));
                let reason = if let Some(column) = column {
                    format!("column {column} differs")
                } else if wanted.3 != actual.3 {
                    format!(
                        "automatic UNIQUE/PRIMARY KEY indexes differ: expected {:?}, found {:?}",
                        wanted.3, actual.3
                    )
                } else {
                    "table options differ".to_owned()
                };
                findings.push(SchemaFinding {
                    object: name,
                    reason,
                });
            }
        } else if kind == "index" || kind == "trigger" {
            derived.push((kind, name, sql));
        }
    }
    drop(statement);
    Ok((derived, findings))
}

fn table_layout(connection: &Connection, name: &str) -> StoreResult<Option<TableLayout>> {
    let options = connection
        .query_row(
            "SELECT wr,strict FROM pragma_table_list WHERE schema='main' AND name=?1 COLLATE NOCASE",
            [name],
            |row| Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0)),
        )
        .optional()?;
    let Some((without_rowid, strict)) = options else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT name,type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo(?1) ORDER BY cid",
    )?;
    let rows = statement.query_map([name], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })?;
    let columns = rows.collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut statement = connection
        .prepare("SELECT name,origin FROM pragma_index_list(?1) WHERE origin IN ('u','pk')")?;
    let indexes = statement
        .query_map([name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut constraints = BTreeSet::new();
    for (index, origin) in indexes {
        let mut statement = connection.prepare(
            "SELECT seqno,cid,name,\"desc\",coll FROM pragma_index_xinfo(?1) WHERE key=1 ORDER BY seqno",
        )?;
        let columns = statement
            .query_map([index], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        constraints.insert((origin, columns));
    }
    Ok(Some((columns, without_rowid, strict, constraints)))
}

fn reconcile_derived_objects(
    transaction: &Transaction<'_>,
    expected: &[CatalogObject],
) -> StoreResult<()> {
    for (kind, expected_name, expected_sql) in expected {
        let existing = transaction
            .query_row(
                "SELECT name,sql FROM sqlite_master WHERE type=?1 AND name=?2 COLLATE NOCASE",
                [kind, expected_name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        match existing {
            Some((_, Some(sql))) if sql == *expected_sql => {}
            Some((name, _)) => {
                transaction.execute_batch(&format!(
                    "DROP {} {}",
                    kind.to_ascii_uppercase(),
                    quote_identifier(&name)
                ))?;
            }
            None => {}
        }
    }
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn schema_object_mismatch(object: &str, reason: &str) -> StoreError {
    StoreError::SchemaObjectMismatch {
        database: "canvas.sqlite3".to_owned(),
        object: object.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_tables_are_registered_in_stable_order() {
        let tables = registered_parent_tables();
        assert_eq!(tables.len(), 3);
        assert!(tables.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn table_inspection_returns_findings_without_writing() {
        let mut connection = Connection::open_in_memory().expect("in-memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_events(id TEXT,agent TEXT,kind TEXT,status TEXT,created REAL,error TEXT);
                 CREATE TABLE runtime_agents(id TEXT);
                 CREATE TABLE runtime_items(agent TEXT);",
            )
            .expect("required parent tables are created");
        let transaction = connection
            .transaction()
            .expect("bootstrap transaction starts");
        prepare_sync_schema(&transaction).expect("sync schema prepares");
        transaction.commit().expect("bootstrap commits");
        assert!(
            inspect_sync_tables(&connection)
                .expect("canonical tables inspect")
                .is_empty()
        );

        connection
            .execute_batch(
                "DROP TABLE sync_identity;
                 CREATE TABLE sync_identity(id VARCHAR PRIMARY KEY);",
            )
            .expect("test table is modified");
        let before = connection
            .pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0))
            .expect("schema version reads");
        let findings = inspect_sync_tables(&connection).expect("table differences inspect");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].object, "sync_identity");
        assert!(findings[0].reason.contains("column id"));
        assert_eq!(
            connection
                .pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0))
                .expect("schema version remains readable"),
            before,
            "inspection is read-only"
        );
    }
}
