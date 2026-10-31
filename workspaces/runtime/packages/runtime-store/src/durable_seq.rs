//! Durable change source: a change number for each row of the main tables, tombstones for
//! deletes.
//!
//! `durable_rows` holds one entry per row `(table, id, seq, deleted)`. A trigger on each
//! opted-in table sets the entry's `seq` to the next number on every insert and update;
//! a delete turns the entry into a tombstone. The main tables keep their own columns, so
//! no writer, fixture or migration changes.
//!
//! The numbers are the counter of `sync_entities`, `sync_documents` and `sync_generation`:
//! a new number is [`NEXT`], one above the highest of those tables and `durable_rows`.
//! Every allocator uses that rule, so there is one cursor space. A reader keeps the
//! highest `seq` it applied and asks [`changes_since`] for what is newer. This replaces
//! the copy of the main rows in `sync_entities`; see docs/sync-protocol.md.
//!
//! Opt in a new table with one line in [`DURABLE_TABLES`]. The table needs an `id`
//! primary key and a text payload column. Installation is idempotent.
//!
//! Tombstones are service data and are kept long. A delete prunes tombstones older than
//! [`TOMBSTONE_RETENTION_SECONDS`] and raises `durable_floor.floor` to the highest pruned
//! `seq`. A cursor below the floor cannot learn every delete, so it gets a snapshot.
//!
//! The DDL of `durable_rows` and `durable_floor` is in `sync_schema.sql` (section
//! `common`), where the allocators that read `durable_rows` are.

use rusqlite::{Connection, OptionalExtension, Transaction};

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
    /// True for a table too large to number in one transaction: the installer adds the
    /// triggers, and an online migration gives the old rows their entries later.
    pub online_backfill: bool,
}

impl DurableTable {
    pub const fn new(name: &'static str, payload: &'static str) -> Self {
        Self {
            name,
            payload,
            online_backfill: false,
        }
    }

    pub const fn online(name: &'static str, payload: &'static str) -> Self {
        Self {
            name,
            payload,
            online_backfill: true,
        }
    }
}

/// Opt-in list. Add one line to publish another table.
pub const DURABLE_TABLES: &[DurableTable] = &[
    DurableTable::new("runtime_agents", "record"),
    DurableTable::new("runtime_requests", "record"),
    DurableTable::new("runtime_work", "record"),
    // The owner of conversation items; transcript pages are built from it at read time.
    DurableTable::online("runtime_items", "record"),
];

/// The next number of the shared counter. Owner of the allocation rule. The same text
/// is in `sync_schema.sql`, `feature_schema.rs` and `sync_documents::next_sequence`.
const NEXT: &str = "(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows))";

/// Derived view collections that the publisher builds from `runtime_agents`,
/// `runtime_requests` and `runtime_work`. Owner of the list. `durable_view_log` mirrors
/// their `sync_entities` rows (collection, id, seq, deleted) without the payload, so a
/// reader can learn which views changed after a cursor from the same cursor space.
pub const VIEW_COLLECTIONS: &[&str] = &[
    "agent",
    "agentSummary",
    "agentDetail",
    "retainedWorktree",
    "questionHistory",
    "request",
    "work",
    "workDetail",
    "workResult",
    "workDecision",
    "workRelease",
];

/// `VIEW_COLLECTIONS` as an SQL list: `'agent','agentSummary',...`.
pub(crate) fn view_collection_list() -> String {
    VIEW_COLLECTIONS
        .iter()
        .map(|collection| format!("'{collection}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// DDL, backfill and mirror triggers of `durable_view_log`; needs `sync_entities`.
pub(crate) fn view_log_sql() -> String {
    let list = view_collection_list();
    let mirror = "INSERT INTO durable_view_log(collection,id,seq,deleted)
         VALUES(NEW.collection,NEW.id,NEW.seq,NEW.deleted)
         ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted;";
    format!(
        "CREATE TABLE IF NOT EXISTS durable_view_log(
             collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
             deleted INTEGER NOT NULL, PRIMARY KEY(collection,id));
         CREATE INDEX IF NOT EXISTS durable_view_log_seq ON durable_view_log(seq);
         INSERT OR IGNORE INTO durable_view_log(collection,id,seq,deleted)
             SELECT collection,id,seq,deleted FROM sync_entities WHERE collection IN ({list});
         CREATE TRIGGER IF NOT EXISTS durable_view_log_insert AFTER INSERT ON sync_entities
         WHEN NEW.collection IN ({list}) BEGIN {mirror} END;
         CREATE TRIGGER IF NOT EXISTS durable_view_log_update AFTER UPDATE ON sync_entities
         WHEN NEW.collection IN ({list}) BEGIN {mirror} END;
         CREATE TRIGGER IF NOT EXISTS durable_view_log_delete AFTER DELETE ON sync_entities
         WHEN OLD.collection IN ({list}) BEGIN
             DELETE FROM durable_view_log WHERE collection=OLD.collection AND id=OLD.id; END;"
    )
}

/// The generator: SQL for the three triggers of one table.
fn trigger_sql(table: &str) -> String {
    let now = "CAST(strftime('%s','now') AS INTEGER)";
    let cutoff = format!("{now}-{TOMBSTONE_RETENTION_SECONDS}");
    let put = |id: &str, deleted: u8, at: &str| {
        format!(
            "INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('{table}',{id},{NEXT},{deleted},{at})
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;"
        )
    };
    let live = put("NEW.id", 0, "0");
    let tombstone = put("OLD.id", 1, now);
    format!(
        "
CREATE TRIGGER IF NOT EXISTS durable_{table}_insert AFTER INSERT ON {table} BEGIN
  {live}
END;
CREATE TRIGGER IF NOT EXISTS durable_{table}_update AFTER UPDATE ON {table} BEGIN
  {live}
  INSERT INTO durable_rows(tbl,id,seq,deleted,at)
    SELECT '{table}',OLD.id,{NEXT},1,{now} WHERE OLD.id IS NOT NEW.id
    ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=1,at=excluded.at;
END;
CREATE TRIGGER IF NOT EXISTS durable_{table}_delete AFTER DELETE ON {table} BEGIN
  UPDATE durable_floor SET floor=MAX(floor,
    (SELECT MAX(seq) FROM durable_rows WHERE deleted=1 AND at<{cutoff}))
  WHERE id=1 AND EXISTS(SELECT 1 FROM durable_rows WHERE deleted=1 AND at<{cutoff});
  DELETE FROM durable_rows WHERE deleted=1 AND at<{cutoff};
  {tombstone}
END;
"
    )
}

/// Number the rows of every opted-in table that have no entry yet (rows that predate
/// the source) and install the triggers. Runs as a migration and on a fresh state.
pub(crate) fn install(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch(&view_log_sql())?;
    for table in DURABLE_TABLES {
        let name = table.name;
        if !table.online_backfill {
            let base: i64 = tx.query_row(&format!("SELECT {NEXT}-1"), [], |r| r.get(0))?;
            tx.execute_batch(&format!(
                "INSERT INTO durable_rows(tbl,id,seq,deleted,at)
                   SELECT '{name}',id,{base}+ROW_NUMBER() OVER (ORDER BY rowid),0,0 FROM {name} t
                   WHERE NOT EXISTS (SELECT 1 FROM durable_rows d WHERE d.tbl='{name}' AND d.id=t.id);"
            ))?;
        }
        tx.execute_batch(&trigger_sql(name))?;
    }
    Ok(())
}

/// Give one row of an `online_backfill` table its entry, unless it has one. Rows written
/// after the triggers exist have an entry from them, so this only reaches old rows. Safe
/// to repeat: a row that has an entry keeps its number.
pub fn number_row(connection: &Connection, table: &str, rowid: i64) -> StoreResult<()> {
    connection.execute(
        &format!(
            "INSERT INTO durable_rows(tbl,id,seq,deleted,at)
               SELECT '{table}',id,{NEXT},0,0 FROM {table} WHERE rowid=?1
             ON CONFLICT(tbl,id) DO NOTHING"
        ),
        [rowid],
    )?;
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
/// The cursor space is shared with `sync_entities`, so `seq` is the highest number of any
/// sync table, and a cursor taken from one of them is valid here. The high mark is read
/// first and later writes are cut off at it, so one call needs no read transaction: a
/// change committed meanwhile has a higher `seq` and arrives with the next call.
pub fn changes_since(
    connection: &Connection,
    table: &str,
    cursor: Option<i64>,
) -> StoreResult<DurableChanges> {
    let table = DURABLE_TABLES
        .iter()
        .find(|candidate| candidate.name == table)
        .ok_or_else(|| StoreError::SchemaMismatch(format!("{table} is not a durable table")))?;
    let high: i64 = connection.query_row(&format!("SELECT {NEXT}-1"), [], |row| row.get(0))?;
    let floor: i64 =
        connection.query_row("SELECT floor FROM durable_floor WHERE id=1", [], |row| {
            row.get(0)
        })?;
    let after = cursor.filter(|cursor| (floor..=high).contains(cursor));
    let rows = |after: i64| -> StoreResult<Vec<DurableRow>> {
        let sql = format!(
            "SELECT t.id,d.seq,t.{} FROM durable_rows d JOIN {} t ON t.id=d.id
             WHERE d.tbl=?1 AND d.deleted=0 AND d.seq>?2 AND d.seq<=?3 ORDER BY d.seq",
            table.payload, table.name
        );
        let rows = connection
            .prepare(&sql)?
            .query_map(rusqlite::params![table.name, after, high], |row| {
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
        return Ok(DurableChanges::Snapshot {
            rows: rows(0)?,
            seq: high,
        });
    };
    let tombstones = connection
        .prepare(
            "SELECT id,seq FROM durable_rows WHERE tbl=?1 AND deleted=1 AND seq>?2 AND seq<=?3
             ORDER BY seq",
        )?
        .query_map(rusqlite::params![table.name, after, high], |row| {
            Ok(DurableTombstone {
                id: row.get(0)?,
                seq: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DurableChanges::Delta {
        rows: rows(after)?,
        tombstones,
        seq: high,
    })
}

/// A view that changed after a cursor. The payload stays in `sync_entities` until the
/// stream builds views at read time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewChange {
    pub collection: String,
    pub id: String,
    pub seq: i64,
    pub deleted: bool,
}

/// Views of [`VIEW_COLLECTIONS`] with `seq` above `cursor`, in `seq` order.
pub fn view_changes_since(connection: &Connection, cursor: i64) -> StoreResult<Vec<ViewChange>> {
    let changes = connection
        .prepare(
            "SELECT collection,id,seq,deleted FROM durable_view_log WHERE seq>?1 ORDER BY seq",
        )?
        .query_map([cursor], |row| {
            Ok(ViewChange {
                collection: row.get(0)?,
                id: row.get(1)?,
                seq: row.get(2)?,
                deleted: row.get::<_, i64>(3)? != 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(changes)
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

/// `sync_entity_meta` key that records the end of the transcript copies in `sync_entities`.
const COPIES_KEY: &str = "transcript_copies";
const COPIES_RETIRED: &str = "retired";

/// True once transcript pages are built at read time and the writer keeps no transcript
/// copy in `sync_entities`.
pub fn transcript_copies_retired(connection: &Connection) -> StoreResult<bool> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM sync_entity_meta WHERE key=?1",
            [COPIES_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.as_deref() == Some(COPIES_RETIRED))
}

/// Stop writing transcript copies, but only when the owner table holds every item the
/// copies list. Returns the missing `(agent, item)` pairs; on a non-empty result nothing
/// changes and the writer keeps the copies. Old rows are never deleted.
pub fn retire_transcript_copies(tx: &Transaction<'_>) -> StoreResult<Vec<(String, String)>> {
    let missing = transcript_copy_missing_from_owner(tx)?;
    if missing.is_empty() {
        tx.execute(
            "INSERT INTO sync_entity_meta(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [COPIES_KEY, COPIES_RETIRED],
        )?;
    }
    Ok(missing)
}

/// The change number of a conversation: the highest `durable_rows.seq` of its agent, its
/// items, and any deleted item. It only grows, and a cursor from any sync table is valid
/// against it because they share one counter.
pub fn transcript_change_cursor(connection: &Connection, agent: &str) -> StoreResult<i64> {
    Ok(connection.query_row(
        "SELECT MAX(
            COALESCE((SELECT seq FROM durable_rows WHERE tbl='runtime_agents' AND id=?1),0),
            COALESCE((SELECT MAX(d.seq) FROM runtime_items i
                      JOIN durable_rows d ON d.tbl='runtime_items' AND d.id=i.id
                      WHERE i.agent=?1),0),
            COALESCE((SELECT MAX(seq) FROM durable_rows
                      WHERE deleted=1 AND tbl='runtime_items'),0))",
        [agent],
        |row| row.get(0),
    )?)
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
        db.query_row(
            "SELECT seq FROM durable_rows WHERE tbl='runtime_work' AND id=?1",
            [id],
            |r| r.get(0),
        )
        .expect("seq")
    }

    fn delta(db: &Connection, cursor: i64) -> (Vec<String>, Vec<String>, i64) {
        match changes_since(db, "runtime_work", Some(cursor)).expect("changes") {
            DurableChanges::Delta {
                rows,
                tombstones,
                seq,
            } => (
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
        db.execute("DELETE FROM runtime_work WHERE id='b'", [])
            .expect("delete");
        let tombstone: i64 = db
            .query_row(
                "SELECT seq FROM durable_rows WHERE tbl='runtime_work' AND id='b' AND deleted=1",
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
        db.execute("DELETE FROM runtime_work WHERE id='b'", [])
            .expect("delete");
        let (mut rows, tombstones, next) = delta(&db, cursor);
        rows.sort();
        assert_eq!(
            (rows, tombstones),
            (vec!["a".into(), "c".into()], vec!["b".into()])
        );
        assert!(next > cursor);
        put(&db, "b", "{}");
        let (rows, tombstones, _) = delta(&db, next);
        assert_eq!(
            (rows, tombstones),
            (vec!["b".into()], vec![]),
            "reinsert clears the tombstone"
        );
    }

    #[test]
    fn a_cursor_older_than_the_kept_tombstones_gets_a_snapshot() {
        let db = canvas();
        put(&db, "a", "{}");
        put(&db, "b", "{}");
        let (_, _, cursor) = delta(&db, 0);
        db.execute("DELETE FROM runtime_work WHERE id='a'", [])
            .expect("delete");
        // Age the tombstone past the retention, then the next delete prunes it.
        db.execute("UPDATE durable_rows SET at=0 WHERE deleted=1", [])
            .expect("age");
        db.execute("DELETE FROM runtime_work WHERE id='b'", [])
            .expect("delete");
        match changes_since(&db, "runtime_work", Some(cursor)).expect("changes") {
            DurableChanges::Snapshot { rows, .. } => assert!(rows.is_empty()),
            other @ DurableChanges::Delta { .. } => panic!("expected a snapshot, got {other:?}"),
        }
        assert!(matches!(
            changes_since(&db, "runtime_work", None).expect("changes"),
            DurableChanges::Snapshot { .. }
        ));
        let high: i64 = db
            .query_row(&format!("SELECT {NEXT}-1"), [], |r| r.get(0))
            .expect("counter");
        assert!(
            matches!(
                changes_since(&db, "runtime_work", Some(high + 1)).expect("changes"),
                DurableChanges::Snapshot { .. }
            ),
            "a future cursor is foreign"
        );
        assert!(matches!(
            changes_since(&db, "runtime_work", Some(high)).expect("changes"),
            DurableChanges::Delta { .. }
        ));
    }

    #[test]
    fn durable_and_sync_writes_share_one_increasing_counter() {
        let db = canvas();
        let next = || crate::repositories::sync_documents::next_sequence(&db).expect("next");
        put(&db, "a", "{}");
        let first = seq_of(&db, "a");
        assert_eq!(
            next(),
            first + 1,
            "a sync allocation sees the durable number"
        );
        let entity = next();
        db.execute(
            "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
             VALUES('x','1',?1,'h','{}',0)",
            [entity],
        )
        .expect("entity");
        put(&db, "b", "{}");
        assert_eq!(
            seq_of(&db, "b"),
            entity + 1,
            "a durable number follows the entity"
        );
        // A cursor taken from the entity stream is valid here and returns only what followed.
        let (rows, _, _) = delta(&db, entity);
        assert_eq!(rows, vec!["b".to_owned()]);
    }

    #[test]
    fn the_view_log_mirrors_view_collections_in_sync_entities() {
        let db = canvas();
        let entity = |collection: &str, id: &str, deleted: i64| {
            db.execute(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                 VALUES(?1,?2,(SELECT MAX(entity_seq,document_seq,generation)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
                    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
                    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1)),
                    'h','{}',?3)
                 ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted",
                rusqlite::params![collection, id, deleted],
            )
            .expect("entity");
        };
        entity("agentSummary", "a", 0);
        entity("event", "e", 0);
        entity("work", "w", 0);
        entity("agentSummary", "a", 1);
        let log = view_changes_since(&db, 0).expect("log");
        assert_eq!(
            log.iter()
                .map(|c| (c.collection.as_str(), c.id.as_str(), c.deleted))
                .collect::<Vec<_>>(),
            vec![("work", "w", false), ("agentSummary", "a", true)],
            "other collections are not logged; an update keeps one row at the new seq"
        );
        let cursor = log[0].seq;
        assert_eq!(view_changes_since(&db, cursor).expect("log").len(), 1);
        db.execute("DELETE FROM sync_entities WHERE collection='work'", [])
            .expect("prune");
        assert_eq!(view_changes_since(&db, 0).expect("log").len(), 1);
    }

    #[test]
    fn install_backfills_the_view_log() {
        let mut db = canvas();
        db.execute_batch(
            "DROP TRIGGER durable_view_log_insert; DROP TABLE durable_view_log;
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
             VALUES('request','r',900,'h','{}',0),('event','e',901,'h','{}',0);",
        )
        .expect("old state");
        let tx = db.transaction().expect("transaction");
        install(&tx).expect("install");
        tx.commit().expect("commit");
        let log = view_changes_since(&db, 0).expect("log");
        assert_eq!(log.len(), 1);
        assert_eq!((log[0].collection.as_str(), log[0].seq), ("request", 900));
    }

    #[test]
    fn an_unknown_table_is_refused() {
        assert!(changes_since(&canvas(), "runtime_events", None).is_err());
    }

    #[test]
    fn install_is_idempotent_and_numbers_rows_that_predate_the_source() {
        let mut db = canvas();
        put(&db, "a", "{}");
        let before = seq_of(&db, "a");
        let tx = db.transaction().expect("transaction");
        install(&tx).expect("second install");
        tx.commit().expect("commit");
        assert_eq!(seq_of(&db, "a"), before);
        // Old format: rows without entries and without triggers.
        db.execute_batch(
            "DROP TRIGGER durable_runtime_work_insert;
             DELETE FROM durable_rows WHERE tbl='runtime_work';
             INSERT INTO runtime_work(id,record) VALUES('old','{}'),('older','{}');",
        )
        .expect("old format");
        let tx = db.transaction().expect("transaction");
        install(&tx).expect("install over old format");
        tx.commit().expect("commit");
        let high: i64 = db
            .query_row(&format!("SELECT {NEXT}-1"), [], |r| r.get(0))
            .expect("counter");
        let (a, old, older) = (seq_of(&db, "a"), seq_of(&db, "old"), seq_of(&db, "older"));
        assert!(a > 0 && old > a && older > old && older <= high);
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

    #[test]
    fn retiring_the_copies_needs_every_item_in_the_owner_and_never_deletes_rows() {
        let mut db = canvas();
        // A fresh state has no copies, so the migration retired them.
        assert!(transcript_copies_retired(&db).expect("flag"));
        db.execute_batch(
            "DELETE FROM sync_entity_meta WHERE key='transcript_copies';
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES
               ('transcript:a1','item:i2',2,'h',NULL,0);",
        )
        .expect("a copy row without its item");
        let tx = db.transaction().expect("transaction");
        let missing = retire_transcript_copies(&tx).expect("check");
        tx.commit().expect("commit");
        assert_eq!(missing, vec![("a1".to_owned(), "i2".to_owned())]);
        assert!(
            !transcript_copies_retired(&db).expect("flag"),
            "the writer keeps the copies"
        );
        db.execute_batch(
            "INSERT INTO runtime_items(id,agent,record,created) VALUES('i2','a1','{}',1)",
        )
        .expect("owner row");
        let tx = db.transaction().expect("transaction");
        assert!(retire_transcript_copies(&tx).expect("check").is_empty());
        tx.commit().expect("commit");
        assert!(transcript_copies_retired(&db).expect("flag"));
        let copies: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sync_entities WHERE collection='transcript:a1'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(copies, 1, "old rows stay");
    }

    #[test]
    fn the_change_cursor_follows_items_agent_and_deletes_of_one_conversation() {
        let db = canvas();
        db.execute_batch(
            "INSERT INTO runtime_agents(id,record) VALUES('a1','{\"provider\":\"codex\",\"id\":\"a1\",\"rootId\":\"a1\",\"epoch\":0,\"status\":\"idle\",\"accountKey\":\"default\"}');
             INSERT INTO runtime_items(id,agent,record,created) VALUES('i1','a1','{}',1);",
        )
        .expect("rows");
        let first = transcript_change_cursor(&db, "a1").expect("cursor");
        assert!(first > 0);
        db.execute(
            "INSERT INTO runtime_items(id,agent,record,created) VALUES('o1','other','{}',1)",
            [],
        )
        .expect("other chat");
        assert_eq!(transcript_change_cursor(&db, "a1").expect("cursor"), first);
        db.execute(
            "UPDATE runtime_items SET record='{\"x\":1}' WHERE id='i1'",
            [],
        )
        .expect("update");
        let second = transcript_change_cursor(&db, "a1").expect("cursor");
        assert!(second > first);
        db.execute("DELETE FROM runtime_items WHERE id='i1'", [])
            .expect("delete");
        assert!(transcript_change_cursor(&db, "a1").expect("cursor") > second);
    }
}
