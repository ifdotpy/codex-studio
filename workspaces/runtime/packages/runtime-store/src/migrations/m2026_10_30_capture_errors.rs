//! Convert historical capture error epoch seconds to the current numeric form.
use crate::{StoreError, StoreResult};
use rusqlite::{OptionalExtension, Transaction, params};

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    let old: Option<String> = tx.query_row("SELECT json_extract(value,'$.last.at') FROM analytics_meta WHERE key='captureErrors' AND json_type(value,'$.last.at')='text'", [], |row| row.get(0)).optional()?;
    if let Some(old) = old {
        let at: f64 = old
            .parse()
            .map_err(|_| StoreError::RecordContract("invalid capture error timestamp"))?;
        if !at.is_finite() || at < 0.0 {
            return Err(StoreError::RecordContract(
                "invalid capture error timestamp",
            ));
        }
        tx.execute("UPDATE analytics_meta SET value=json_set(value,'$.last.at',?1) WHERE key='captureErrors'", params![at])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    #[ignore]
    fn measure_capture_error_migration_on_explicit_copy() {
        let path =
            std::env::var_os("STUDIO_CAPTURE_COPY").expect("explicit disposable database copy");
        let mut db = Connection::open(path).expect("database copy");
        let start = std::time::Instant::now();
        let tx = db.transaction().expect("transaction");
        apply(&tx).expect("migration");
        tx.commit().expect("commit");
        eprintln!("capture error migration elapsed: {:?}", start.elapsed());
        let kind: String = db
            .query_row(
                "SELECT json_type(value,'$.last.at') FROM analytics_meta WHERE key='captureErrors'",
                [],
                |row| row.get(0),
            )
            .expect("record");
        assert_eq!(kind, "real");
    }

    #[tokio::test]
    async fn startup_converts_capture_error_and_refuses_invalid_legacy_time() {
        let root = tempfile::tempdir().expect("state");
        drop(
            crate::RuntimeStore::open(Some(root.path()))
                .await
                .expect("fresh"),
        );
        let db = Connection::open(root.path().join("analytics.sqlite3")).expect("analytics");
        db.execute(
            "INSERT INTO analytics_meta VALUES('captureErrors',?1)",
            [r#"{"count":2,"last":{"at":"1792320464","operation":"capture","error":"failure"}}"#],
        )
        .expect("legacy");
        db.execute(
            "DELETE FROM analytics_meta WHERE key='migration_catalog'",
            [],
        )
        .expect("legacy catalog");
        for _ in 0..2 {
            let store = crate::RuntimeStore::open(Some(root.path()))
                .await
                .expect("startup");
            let summary = store
                .with_cost_reader(|db| crate::analytics::capture_error_summary(db))
                .expect("summary");
            assert_eq!(summary.count, 2);
            assert_eq!(summary.last.expect("last").error, "failure");
            drop(store);
        }
        db.execute("UPDATE analytics_meta SET value=json_set(value,'$.last.at','bad') WHERE key='captureErrors'", []).expect("invalid");
        db.execute(
            "DELETE FROM analytics_meta WHERE key='migration_catalog'",
            [],
        )
        .expect("legacy catalog");
        assert!(crate::RuntimeStore::open(Some(root.path())).await.is_err());
        let value: String = db.query_row("SELECT json_extract(value,'$.last.at') FROM analytics_meta WHERE key='captureErrors'", [], |row| row.get(0)).expect("source");
        assert_eq!(value, "bad");
    }

    #[test]
    fn capture_error_migration_converts_once_and_rejects_bad_timestamp() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE analytics_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO analytics_meta VALUES('captureErrors','{\"count\":2,\"last\":{\"at\":\"1792320464\",\"error\":\"failure\"}}');").expect("fixture");
        for _ in 0..2 {
            let tx = db.transaction().expect("transaction");
            apply(&tx).expect("migration");
            tx.commit().expect("commit");
            let result: (String, f64) = db.query_row("SELECT json_type(value,'$.last.at'), json_extract(value,'$.last.at') FROM analytics_meta", [], |row| Ok((row.get(0)?, row.get(1)?))).expect("row");
            assert_eq!(result, ("real".to_owned(), 1_792_320_464.0));
        }
        db.execute(
            "UPDATE analytics_meta SET value=json_set(value,'$.last.at','invalid')",
            [],
        )
        .expect("invalid fixture");
        let tx = db.transaction().expect("transaction");
        assert!(apply(&tx).is_err());
        tx.rollback().expect("rollback");
        let value: String = db
            .query_row(
                "SELECT json_extract(value,'$.last.at') FROM analytics_meta",
                [],
                |row| row.get(0),
            )
            .expect("source");
        assert_eq!(value, "invalid");
    }
}
