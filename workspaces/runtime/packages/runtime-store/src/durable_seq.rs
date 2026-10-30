//! Durable change source: a `seq` column on each main table, tombstones for deletes.
//!
//! One global counter (`durable_counter.value`) numbers every insert, update and delete
//! of every opted-in table. A trigger sets the row's `seq` to the next number; a delete
//! writes a tombstone `(table, id, seq)` instead. A reader keeps the highest `seq` it
//! applied (its cursor) and asks [`changes_since`] for what is newer. This replaces the
//! copy of the main rows in `sync_entities`; see docs/sync-protocol.md "Durable stream".
//!
//! Opt in a new table with one line in [`DURABLE_TABLES`]. The table needs an `id`
//! primary key and a text payload column; the installer adds `seq`, its index and the
//! three triggers. Installation is idempotent.
//!
//! Tombstones are service data and are kept long. A delete trigger prunes tombstones
//! older than [`TOMBSTONE_RETENTION_SECONDS`] and raises `durable_counter.floor` to the
//! highest pruned `seq`. A cursor below the floor cannot learn every delete, so it gets
//! a snapshot.

use rusqlite::{Connection, Transaction};

use crate::{StoreError, StoreResult};

/// How long a tombstone stays, in seconds (90 days). A reconnect inside this window gets a
/// delta; a longer absence gets a snapshot.
pub const TOMBSTONE_RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;

/// A main table that feeds the durable stream.
pub struct DurableTable {
    /// Table name; the table has `id TEXT PRIMARY KEY` and `rowid`.
    pub name: &'static str,
    /// Column that holds the row payload sent to readers.
    pub payload: &'static str,
}

impl DurableTable {
    pub const fn new(name: &'static str, payload: &'static str) -> Self {
        Self { name, payload }
    }
}

/// Opt-in list. Add one line to publish another table.
pub const DURABLE_TABLES: &[DurableTable] = &[
    DurableTable::new("runtime_agents", "record"),
    DurableTable::new("runtime_requests", "record"),
    DurableTable::new("runtime_work", "record"),
];

const SHARED_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS durable_counter(
    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL, floor INTEGER NOT NULL);
INSERT OR IGNORE INTO durable_counter(id,value,floor) VALUES(1,0,0);
CREATE TABLE IF NOT EXISTS durable_tombstones(
    tbl TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL,
    PRIMARY KEY(tbl,id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS durable_tombstones_seq ON durable_tombstones(seq);
CREATE INDEX IF NOT EXISTS durable_tombstones_at ON durable_tombstones(at);
";

/// The generator: SQL for the three triggers of one table.
fn trigger_sql(table: &str) -> String {
    let next = "UPDATE durable_counter SET value=value+1 WHERE id=1;";
    let current = "(SELECT value FROM durable_counter WHERE id=1)";
    let now = "CAST(strftime('%s','now') AS INTEGER)";
    let cutoff = format!("{now}-{TOMBSTONE_RETENTION_SECONDS}");
    format!(
        "
CREATE TRIGGER IF NOT EXISTS durable_{table}_insert AFTER INSERT ON {table} BEGIN
  DELETE FROM durable_tombstones WHERE tbl='{table}' AND id=NEW.id;
  {next}
  UPDATE {table} SET seq={current} WHERE rowid=NEW.rowid;
END;
CREATE TRIGGER IF NOT EXISTS durable_{table}_update AFTER UPDATE ON {table}
WHEN NEW.seq IS OLD.seq BEGIN
  {next}
  UPDATE {table} SET seq={current} WHERE rowid=NEW.rowid;
  INSERT OR REPLACE INTO durable_tombstones(tbl,id,seq,at)
    SELECT '{table}',OLD.id,{current},{now} WHERE OLD.id IS NOT NEW.id;
END;
CREATE TRIGGER IF NOT EXISTS durable_{table}_delete AFTER DELETE ON {table} BEGIN
  UPDATE durable_counter SET floor=MAX(floor,
    (SELECT MAX(seq) FROM durable_tombstones WHERE at<{cutoff}))
  WHERE id=1 AND EXISTS(SELECT 1 FROM durable_tombstones WHERE at<{cutoff});
  DELETE FROM durable_tombstones WHERE at<{cutoff};
  {next}
  INSERT OR REPLACE INTO durable_tombstones(tbl,id,seq,at)
    VALUES('{table}',OLD.id,{current},{now});
END;
"
    )
}

/// Add `seq` to every opted-in table, number the existing rows once, and install the
/// triggers. Runs as a migration and on a fresh state.
pub(crate) fn install(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch(SHARED_SCHEMA)?;
    for table in DURABLE_TABLES {
        let name = table.name;
        let has_seq: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name='seq')",
            [name],
            |row| row.get(0),
        )?;
        if !has_seq {
            tx.execute_batch(&format!(
                "ALTER TABLE {name} ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;"
            ))?;
            number_existing_rows(tx, name)?;
        }
        tx.execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS durable_{name}_seq ON {name}(seq);{}",
            trigger_sql(name)
        ))?;
    }
    Ok(())
}

/// Give rows that predate the column increasing numbers, before the triggers exist.
fn number_existing_rows(tx: &Transaction<'_>, table: &str) -> StoreResult<()> {
    let mut value: i64 =
        tx.query_row("SELECT value FROM durable_counter WHERE id=1", [], |r| r.get(0))?;
    let rowids = tx
        .prepare(&format!("SELECT rowid FROM {table} ORDER BY rowid"))?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for rowid in rowids {
        value += 1;
        tx.execute(
            &format!("UPDATE {table} SET seq=?1 WHERE rowid=?2"),
            [value, rowid],
        )?;
    }
    tx.execute("UPDATE durable_counter SET value=?1 WHERE id=1", [value])?;
    Ok(())
}

/// A live row at or below the returned cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRow {
    pub id: String,
    pub seq: i64,
    pub payload: String,
}

/// A deleted id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTombstone {
    pub id: String,
    pub seq: i64,
}

/// What a subscription receives. `seq` is the new cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableChanges {
    /// Every live row; the reader replaces its copy.
    Snapshot { rows: Vec<DurableRow>, seq: i64 },
    /// Rows and deletes newer than the cursor.
    Delta {
        rows: Vec<DurableRow>,
        tombstones: Vec<DurableTombstone>,
        seq: i64,
    },
}

/// Rows of `table` with `seq` above `cursor`, or a snapshot when the cursor is absent,
/// in the future, or below the tombstone floor.
///
/// The counter is read first and later writes are cut off at it, so one call needs no
/// read transaction: a change committed meanwhile has a higher `seq` and arrives with
/// the next call.
pub fn changes_since(
    connection: &Connection,
    table: &str,
    cursor: Option<i64>,
) -> StoreResult<DurableChanges> {
    let table = DURABLE_TABLES
        .iter()
        .find(|candidate| candidate.name == table)
        .ok_or_else(|| StoreError::SchemaMismatch(format!("{table} is not a durable table")))?;
    let (high, floor): (i64, i64) = connection.query_row(
        "SELECT value,floor FROM durable_counter WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let after = cursor.filter(|cursor| (floor..=high).contains(cursor));
    let rows = |after: i64| -> StoreResult<Vec<DurableRow>> {
        let sql = format!(
            "SELECT id,seq,{} FROM {} WHERE seq>?1 AND seq<=?2 ORDER BY seq",
            table.payload, table.name
        );
        let rows = connection
            .prepare(&sql)?
            .query_map([after, high], |row| {
                Ok(DurableRow {
                    id: row.get(0)?,
                    seq: row.get(1)?,
                    payload: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    };
    let Some(after) = after else {
        return Ok(DurableChanges::Snapshot { rows: rows(0)?, seq: high });
    };
    let tombstones = connection
        .prepare(
            "SELECT id,seq FROM durable_tombstones WHERE tbl=?1 AND seq>?2 AND seq<=?3 ORDER BY seq",
        )?
        .query_map(rusqlite::params![table.name, after, high], |row| {
            Ok(DurableTombstone { id: row.get(0)?, seq: row.get(1)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DurableChanges::Delta { rows: rows(after)?, tombstones, seq: high })
}

/// Data-loss check for removing the transcript copy in `sync_entities`: ids of live
/// `transcript:<agent>` index rows whose item is missing from the owner table
/// `runtime_items`. Empty means the owner holds every item the copy lists.
pub fn transcript_copy_missing_from_owner(
    connection: &Connection,
) -> StoreResult<Vec<(String, String)>> {
    let missing = connection
        .prepare(
            "SELECT substr(s.collection,12),substr(s.id,6) FROM sync_entities s
             WHERE s.collection LIKE 'transcript:%' AND s.id LIKE 'item:%' AND s.deleted=0
               AND NOT EXISTS (SELECT 1 FROM runtime_items i
                               WHERE i.agent=substr(s.collection,12) AND i.id=substr(s.id,6))
             ORDER BY s.collection,s.id",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    fn canvas() -> Connection {
        crate::fresh_state::test_canvas()
    }

    fn put(db: &Connection, id: &str, record: &str) {
        db.execute(
            "INSERT INTO runtime_work(id,record) VALUES(?1,?2)
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            [id, record],
        )
        .expect("write");
    }

    fn seq_of(db: &Connection, id: &str) -> i64 {
        db.query_row("SELECT seq FROM runtime_work WHERE id=?1", [id], |r| r.get(0))
            .expect("seq")
    }

    fn delta(db: &Connection, cursor: i64) -> (Vec<String>, Vec<String>, i64) {
        match changes_since(db, "runtime_work", Some(cursor)).expect("changes") {
            DurableChanges::Delta { rows, tombstones, seq } => (
                rows.into_iter().map(|row| row.id).collect(),
                tombstones.into_iter().map(|t| t.id).collect(),
                seq,
            ),
            DurableChanges::Snapshot { .. } => panic!("expected a delta"),
        }
    }

    #[test]
    fn insert_update_and_delete_take_increasing_numbers() {
        let db = canvas();
        put(&db, "a", "{}");
        let first = seq_of(&db, "a");
        put(&db, "b", "{}");
        let second = seq_of(&db, "b");
        put(&db, "a", r#"{"x":1}"#);
        let updated = seq_of(&db, "a");
        assert!(first > 0 && second > first && updated > second);
        db.execute("DELETE FROM runtime_work WHERE id='b'", []).expect("delete");
        let tombstone: i64 = db
            .query_row(
                "SELECT seq FROM durable_tombstones WHERE tbl='runtime_work' AND id='b'",
                [],
                |r| r.get(0),
            )
            .expect("tombstone");
        assert!(tombstone > updated);
    }

    #[test]
    fn a_cursor_resume_returns_exactly_the_changes() {
        let db = canvas();
        put(&db, "a", "{}");
        put(&db, "b", "{}");
        let (_, _, cursor) = delta(&db, 0);
        assert_eq!(delta(&db, cursor), (vec![], vec![], cursor), "nothing new");
        put(&db, "c", "{}");
        put(&db, "a", r#"{"x":1}"#);
        db.execute("DELETE FROM runtime_work WHERE id='b'", []).expect("delete");
        let (mut rows, tombstones, next) = delta(&db, cursor);
        rows.sort();
        assert_eq!((rows, tombstones), (vec!["a".into(), "c".into()], vec!["b".into()]));
        assert!(next > cursor);
        put(&db, "b", "{}");
        let (rows, tombstones, _) = delta(&db, next);
        assert_eq!((rows, tombstones), (vec!["b".into()], vec![]), "reinsert clears the tombstone");
    }

    #[test]
    fn a_cursor_older_than_the_kept_tombstones_gets_a_snapshot() {
        let db = canvas();
        put(&db, "a", "{}");
        put(&db, "b", "{}");
        let (_, _, cursor) = delta(&db, 0);
        db.execute("DELETE FROM runtime_work WHERE id='a'", []).expect("delete");
        // Age the tombstone past the retention, then the next delete prunes it.
        db.execute("UPDATE durable_tombstones SET at=0", []).expect("age");
        db.execute("DELETE FROM runtime_work WHERE id='b'", []).expect("delete");
        match changes_since(&db, "runtime_work", Some(cursor)).expect("changes") {
            DurableChanges::Snapshot { rows, .. } => assert!(rows.is_empty()),
            other => panic!("expected a snapshot, got {other:?}"),
        }
        assert!(matches!(
            changes_since(&db, "runtime_work", None).expect("changes"),
            DurableChanges::Snapshot { .. }
        ));
        let high: i64 = db
            .query_row("SELECT value FROM durable_counter", [], |r| r.get(0))
            .expect("counter");
        assert!(matches!(
            changes_since(&db, "runtime_work", Some(high + 1)).expect("changes"),
            DurableChanges::Snapshot { .. }
        ), "a future cursor is foreign");
        assert!(matches!(
            changes_since(&db, "runtime_work", Some(high)).expect("changes"),
            DurableChanges::Delta { .. }
        ));
    }

    #[test]
    fn an_unknown_table_is_refused() {
        assert!(changes_since(&canvas(), "runtime_items", None).is_err());
    }

    #[test]
    fn install_is_idempotent_and_numbers_rows_that_predate_the_column() {
        let mut db = canvas();
        put(&db, "a", "{}");
        let before = seq_of(&db, "a");
        let tx = db.transaction().expect("transaction");
        install(&tx).expect("second install");
        tx.commit().expect("commit");
        assert_eq!(seq_of(&db, "a"), before);
        // Old format: a table without the column and triggers.
        db.execute_batch(
            "DROP TRIGGER durable_runtime_work_insert;
             DROP TRIGGER durable_runtime_work_update;
             DROP TRIGGER durable_runtime_work_delete;
             DROP INDEX durable_runtime_work_seq;
             ALTER TABLE runtime_work DROP COLUMN seq;
             INSERT INTO runtime_work(id,record) VALUES('old','{}');",
        )
        .expect("old format");
        let tx = db.transaction().expect("transaction");
        install(&tx).expect("install over old format");
        tx.commit().expect("commit");
        let high: i64 = db
            .query_row("SELECT value FROM durable_counter", [], |r| r.get(0))
            .expect("counter");
        assert!(seq_of(&db, "a") > 0 && seq_of(&db, "old") > 0);
        assert!(seq_of(&db, "a") <= high && seq_of(&db, "old") <= high);
    }

    #[test]
    fn the_transcript_copy_check_names_items_the_owner_lacks() {
        let db = canvas();
        db.execute_batch(
            "INSERT INTO runtime_items(id,agent,record,created) VALUES('i1','a1','{}',1);
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES
               ('transcript:a1','item:i1',1,'h',NULL,0),
               ('transcript:a1','item:i2',2,'h',NULL,0),
               ('transcript:a1','item:i3',3,'h',NULL,1),
               ('agent','a1',4,'h','{}',0);",
        )
        .expect("rows");
        assert_eq!(
            transcript_copy_missing_from_owner(&db).expect("check"),
            vec![("a1".to_owned(), "i2".to_owned())]
        );
    }
}
