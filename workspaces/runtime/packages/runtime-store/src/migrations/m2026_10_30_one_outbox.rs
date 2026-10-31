//! Move the pending rows of the reservation outboxes into the shared outbox.
//!
//! One transaction adds the `target` column, copies each recognized pending row
//! of `runtime_access_outbound` and `runtime_server_fetch` into `runtime_outbox`
//! (same request id, same signature, same record), and deletes the copied source
//! row. A row that is not recognized stays in its old table untouched. The old
//! tables are created as before, but nothing writes them any more.
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::OutboxStatus;

use crate::StoreResult;
use crate::repositories::remote_outbox::{RemoteEffect, RemoteOutbox};

fn has_table(tx: &Transaction<'_>, name: &str) -> StoreResult<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

fn has_column(tx: &Transaction<'_>, table: &str, column: &str) -> StoreResult<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
        params![table, column],
        |row| row.get(0),
    )?)
}

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if has_table(tx, "runtime_outbox")? && !has_column(tx, "runtime_outbox", "target")? {
        tx.execute_batch("ALTER TABLE runtime_outbox ADD COLUMN target TEXT")?;
    }
    crate::receipt_schema::create(tx)?;
    move_access_requests(tx)?;
    move_server_fetches(tx)
}

/// A reservation of a paired-server request. The outcome is unknown until it is settled.
fn move_access_requests(tx: &Transaction<'_>) -> StoreResult<()> {
    if !has_table(tx, "runtime_access_outbound")? {
        return Ok(());
    }
    let rows = {
        let mut query = tx.prepare(
            "SELECT rowid,server,request_id,hash,record FROM runtime_access_outbound ORDER BY rowid",
        )?;
        query
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (rowid, server, request, signature, record) in rows {
        let Ok(record) = serde_json::from_str::<Value>(&record) else {
            continue;
        };
        let (Some("unknown"), Some(created), true) = (
            record["outcome"].as_str(),
            record["created"].as_f64(),
            record["admission"].is_object(),
        ) else {
            continue;
        };
        RemoteOutbox::put(
            tx,
            RemoteEffect::AccessRequest,
            &server,
            &request,
            &signature,
            OutboxStatus::Submitted,
            None,
            &record,
            (created * 1000.0) as i64,
        )?;
        tx.execute(
            "DELETE FROM runtime_access_outbound WHERE rowid=?1",
            [rowid],
        )?;
    }
    Ok(())
}

/// A local Git fetch from a paired server. `preparing` has sent nothing; `fetching` has.
fn move_server_fetches(tx: &Transaction<'_>) -> StoreResult<()> {
    if !has_table(tx, "runtime_server_fetch")?
        || !has_column(tx, "runtime_server_fetch", "admission")?
    {
        return Ok(());
    }
    let rows = {
        let mut query = tx.prepare(
            "SELECT rowid,id,server,signature,state,result,admission FROM runtime_server_fetch \
             WHERE admission IS NOT NULL ORDER BY rowid",
        )?;
        query
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (rowid, id, server, signature, state, export, admission) in rows {
        let status = match state.as_str() {
            "preparing" => OutboxStatus::Pending,
            "fetching" => OutboxStatus::Submitted,
            _ => continue,
        };
        let Ok(admission) = serde_json::from_str::<Value>(&admission) else {
            continue;
        };
        if !admission.is_object() {
            continue;
        }
        RemoteOutbox::put(
            tx,
            RemoteEffect::ServerFetch,
            &server,
            &id,
            &signature,
            status,
            None,
            json!({"state": state, "export": export, "admission": admission}),
            0,
        )?;
        tx.execute("DELETE FROM runtime_server_fetch WHERE rowid=?1", [rowid])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    const ADMISSION: &str = r#"{"id":"owner","signature":"sig","created":1}"#;

    /// The outbox table before the `target` column, with the old transport tables.
    fn old_database() -> Connection {
        let db = Connection::open_in_memory().expect("database");
        db.execute_batch(
            "CREATE TABLE runtime_outbox(id TEXT PRIMARY KEY, kind TEXT NOT NULL, owner_kind TEXT NOT NULL,
                owner_id TEXT NOT NULL, request_id TEXT, signature TEXT NOT NULL, status TEXT NOT NULL,
                account TEXT, stream_id TEXT, generation INTEGER, ordinal INTEGER, next_at INTEGER,
                payload TEXT NOT NULL, created INTEGER NOT NULL);
             CREATE TABLE runtime_access_outbound(server TEXT NOT NULL, request_id TEXT NOT NULL,
                hash TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(server,request_id));
             CREATE TABLE runtime_server_fetch(id TEXT PRIMARY KEY, server TEXT NOT NULL,
                signature TEXT NOT NULL, state TEXT NOT NULL, result TEXT, admission TEXT);",
        )
        .expect("old schema");
        db.execute(
            "INSERT INTO runtime_access_outbound VALUES('peer','access-1','hash-1',?1)",
            [json!({"outcome":"unknown","created":2.5,"admission":serde_json::from_str::<Value>(ADMISSION).unwrap()}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO runtime_access_outbound VALUES('peer','access-odd','hash-2','{\"outcome\":\"bogus\"}')",
            [],
        )
        .unwrap();
        for (id, state) in [("fetch-1", "preparing"), ("fetch-2", "fetching")] {
            db.execute(
                "INSERT INTO runtime_server_fetch VALUES(?1,'peer','sig',?2,'{\"cwd\":null}',?3)",
                params![id, state, ADMISSION],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn pending_rows_move_once_with_their_ids_and_signatures() {
        let mut db = old_database();
        for _ in 0..2 {
            let tx = db.transaction().unwrap();
            apply(&tx).expect("migration applies");
            tx.commit().unwrap();
        }
        let get = |effect, id: &str| {
            RemoteOutbox::get::<Value>(&db, effect, "peer", id)
                .unwrap()
                .expect("moved entry")
        };
        let access = get(RemoteEffect::AccessRequest, "access-1");
        assert_eq!(access.signature, "hash-1");
        assert_eq!(access.status, OutboxStatus::Submitted);
        assert_eq!(access.record["outcome"], "unknown");
        assert_eq!(
            get(RemoteEffect::ServerFetch, "fetch-1").status,
            OutboxStatus::Pending
        );
        let fetching = get(RemoteEffect::ServerFetch, "fetch-2");
        assert_eq!(fetching.status, OutboxStatus::Submitted);
        assert_eq!(fetching.record["export"], "{\"cwd\":null}");
        let count = |sql: &str| db.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT count(*) FROM runtime_outbox"), 3);
        assert_eq!(count("SELECT count(*) FROM runtime_server_fetch"), 0);
        // The unrecognized row stays where it was.
        assert_eq!(count("SELECT count(*) FROM runtime_access_outbound"), 1);
    }
}
