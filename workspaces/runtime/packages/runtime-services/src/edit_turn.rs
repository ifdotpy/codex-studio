//! `edit(turn)`: rewind a chat to the end of an earlier turn, for every provider.
//!
//! The steps, in order (decision 2026-10-28, "One command path"):
//! 1. Check that the target checkpoint is valid and fork the provider thread from
//!    it through the provider adapter ([`crate::provider_adapter::ProviderAdapter::edit_thread_request`]).
//! 2. Snapshot the current files, then restore the files of the turn
//!    ([`snapshot_then_restore`]). The snapshot always comes first, so a restore
//!    cannot lose work.
//! 3. Hide the items after the turn ([`set_item_visibility`]). An item is never deleted.
//!
//! The durable workflow (reservation, phases, recovery after a lost response) is the
//! checkpoint restore of the HTTP layer. This module owns the rules that apply to every
//! provider, so a test can check them without a provider process or a Git repository.

use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;

#[derive(Debug, thiserror::Error)]
pub enum EditTurnError {
    #[error("the item store failed: {0}")]
    Store(#[from] rusqlite::Error),
    #[error("an item record is not JSON")]
    Record(#[from] serde_json::Error),
}

/// Hide every item of `agent` that is not in `visible`, and show every hidden item that
/// is. A hidden item keeps its row and gets `afterRestore = marker`; readers skip it.
/// Returns the number of items that changed.
pub fn set_item_visibility(
    connection: &Connection,
    agent: &str,
    visible: &HashSet<String>,
    marker: &Value,
) -> Result<usize, EditTurnError> {
    let mut statement =
        connection.prepare("SELECT id,record FROM runtime_items WHERE agent=?1 ORDER BY rowid")?;
    let rows = statement
        .query_map([agent], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut changed = 0;
    for (id, raw) in rows {
        let mut item: Value = serde_json::from_str(&raw)?;
        let shown = visible.contains(&id);
        let was_shown = item.get("afterRestore").is_none();
        if shown == was_shown {
            continue;
        }
        if shown {
            if let Some(object) = item.as_object_mut() {
                object.remove("afterRestore");
            }
        } else {
            item["afterRestore"] = marker.clone();
        }
        connection.execute(
            "UPDATE runtime_items SET record=?1 WHERE id=?2 AND agent=?3",
            params![item.to_string(), id, agent],
        )?;
        changed += 1;
    }
    Ok(changed)
}

/// Run `snapshot`, then `restore`. `restore` does not start when `snapshot` fails.
/// Both are closures so that neither starts before its turn.
pub async fn snapshot_then_restore<E, S, R>(
    snapshot: impl FnOnce() -> S,
    restore: impl FnOnce() -> R,
) -> Result<(), E>
where
    S: Future<Output = Result<(), E>>,
    R: Future<Output = Result<(), E>>,
{
    snapshot().await?;
    restore().await
}

#[cfg(test)]
mod tests {
    use super::{set_item_visibility, snapshot_then_restore};
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::collections::HashSet;

    fn items(ids: &[&str]) -> Connection {
        let connection = Connection::open_in_memory().expect("memory database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_items(id TEXT, agent TEXT, record TEXT, created REAL);",
            )
            .expect("table");
        for id in ids {
            connection
                .execute(
                    "INSERT INTO runtime_items VALUES(?1,'a',?2,0)",
                    rusqlite::params![id, json!({"id": id, "text": id}).to_string()],
                )
                .expect("item");
        }
        connection
    }

    fn records(connection: &Connection) -> Vec<Value> {
        let mut statement = connection
            .prepare("SELECT record FROM runtime_items ORDER BY rowid")
            .expect("prepare");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .map(|raw| serde_json::from_str(&raw.expect("row")).expect("json"))
            .collect()
    }

    #[test]
    fn items_after_the_turn_are_hidden_and_never_deleted() {
        let connection = items(&["u1", "a1", "u2", "a2"]);
        let visible: HashSet<String> = ["u1", "a1"].map(String::from).into();
        let changed = set_item_visibility(&connection, "a", &visible, &json!("cp1")).expect("hide");
        assert_eq!(changed, 2);
        let rows = records(&connection);
        assert_eq!(rows.len(), 4, "no row is deleted");
        assert!(rows[0].get("afterRestore").is_none());
        assert!(rows[1].get("afterRestore").is_none());
        assert_eq!(rows[2]["afterRestore"], "cp1");
        assert_eq!(rows[3]["afterRestore"], "cp1");
        assert_eq!(rows[3]["text"], "a2", "the content stays");
    }

    #[test]
    fn a_later_rewind_shows_hidden_items_again() {
        let connection = items(&["u1", "a1", "u2"]);
        let first: HashSet<String> = ["u1"].map(String::from).into();
        set_item_visibility(&connection, "a", &first, &json!("cp1")).expect("hide");
        let all: HashSet<String> = ["u1", "a1", "u2"].map(String::from).into();
        let changed = set_item_visibility(&connection, "a", &all, &json!("cp2")).expect("show");
        assert_eq!(changed, 2);
        assert!(
            records(&connection)
                .iter()
                .all(|row| row.get("afterRestore").is_none())
        );
    }

    #[test]
    fn another_agents_items_are_not_touched() {
        let connection = items(&["u1"]);
        connection
            .execute(
                "INSERT INTO runtime_items VALUES('x','b','{\"id\":\"x\"}',9)",
                [],
            )
            .expect("other agent");
        set_item_visibility(&connection, "a", &HashSet::new(), &json!("cp1")).expect("hide");
        let rows = records(&connection);
        assert!(rows[1].get("afterRestore").is_none());
    }

    #[tokio::test]
    async fn the_snapshot_runs_before_the_restore() {
        let log = RefCell::new(Vec::new());
        let log_ref = &log;
        let result: Result<(), &str> = snapshot_then_restore(
            || async move {
                log_ref.borrow_mut().push("snapshot");
                Ok(())
            },
            || async move {
                log_ref.borrow_mut().push("restore");
                Ok(())
            },
        )
        .await;
        assert_eq!(result, Ok(()));
        assert_eq!(*log.borrow(), ["snapshot", "restore"]);
    }

    #[tokio::test]
    async fn a_failed_snapshot_stops_the_restore() {
        let log = RefCell::new(Vec::new());
        let log_ref = &log;
        let result: Result<(), &str> = snapshot_then_restore(
            || async move {
                log_ref.borrow_mut().push("snapshot");
                Err("disk full")
            },
            || async move {
                log_ref.borrow_mut().push("restore");
                Ok(())
            },
        )
        .await;
        assert_eq!(result, Err("disk full"));
        assert_eq!(*log.borrow(), ["snapshot"]);
    }
}
