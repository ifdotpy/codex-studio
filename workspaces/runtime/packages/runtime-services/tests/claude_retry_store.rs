use serde_json::Value;
use studio_runtime_services::claude_input_recovery::{
    Rejection, TransportSnapshot, store::recover_rejected_start,
};
fn state(db: &rusqlite::Connection) -> Value {
    let mut result = serde_json::Map::new();
    for table in [
        "runtime_agents",
        "runtime_execution_attempts",
        "runtime_events",
        "runtime_event_meta",
    ] {
        let mut statement = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY id"))
            .expect("valid fixture");
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                let mut values = vec![];
                for i in 0..columns {
                    values.push(format!("{:?}", row.get_ref(i)?));
                }
                Ok(values)
            })
            .expect("valid fixture")
            .collect::<Result<Vec<_>, _>>()
            .expect("valid fixture");
        result.insert(table.into(), serde_json::json!(rows));
    }
    Value::Object(result)
}
#[test]
fn transaction_failpoints_never_publish_partial_retry() {
    let directory = tempfile::tempdir().expect("valid fixture");
    let template = directory.path().join("template.sqlite3");
    let output =
        std::process::Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("valid fixture"))
            .args([
                "-B",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/claude_runtime/oracle.py"
                ),
                "--template",
            ])
            .arg(&template)
            .output()
            .expect("valid fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let corpus: Vec<Value> = serde_json::from_slice(&output.stdout).expect("valid fixture");
    let input = &corpus[0]["input"];
    for fail in [
        "runtime_agents",
        "runtime_execution_attempts",
        "runtime_event_meta",
        "runtime_events",
        "before_commit",
        "after_commit",
    ] {
        let path = directory.path().join(format!("{fail}.sqlite3"));
        std::fs::copy(&template, &path).expect("valid fixture");
        let mut db = rusqlite::Connection::open(&path).expect("valid fixture");
        let before = state(&db);
        if fail.starts_with("runtime_") {
            db.execute_batch(&format!("CREATE TRIGGER failpoint AFTER UPDATE ON {fail} BEGIN SELECT RAISE(ABORT,'fixture failpoint'); END;")).expect("valid fixture");
        }
        let tx = db.transaction().expect("valid fixture");
        let outcome = recover_rejected_start(
            &tx,
            "agent",
            "attempt",
            serde_json::from_value::<TransportSnapshot>(input["transport"].clone())
                .expect("valid fixture"),
            false,
            &serde_json::from_value::<Rejection>(input["rejection"].clone())
                .expect("valid fixture"),
        );
        if fail.starts_with("runtime_") {
            assert!(outcome.is_err(), "{fail}");
            drop(tx);
            assert_eq!(state(&db), before, "{fail}");
            db.execute_batch("DROP TRIGGER failpoint")
                .expect("valid fixture");
        } else if fail == "before_commit" {
            assert!(outcome.expect("valid fixture").is_some());
            drop(tx);
            assert_eq!(state(&db), before);
        } else {
            assert!(outcome.expect("valid fixture").is_some());
            tx.commit().expect("valid fixture");
        }
        drop(db);
        let mut db = rusqlite::Connection::open(&path).expect("valid fixture");
        let tx = db.transaction().expect("valid fixture");
        let outcome = recover_rejected_start(
            &tx,
            "agent",
            "attempt",
            serde_json::from_value(input["transport"].clone()).expect("valid fixture"),
            false,
            &serde_json::from_value(input["rejection"].clone()).expect("valid fixture"),
        )
        .expect("valid fixture");
        assert_eq!(outcome.is_some(), fail != "after_commit");
        tx.commit().expect("valid fixture");
        let agent: String = db
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |r| r.get(0),
            )
            .expect("valid fixture");
        assert_eq!(
            serde_json::from_str::<Value>(&agent).expect("valid fixture"),
            corpus[0]["expected"]["agent"]
        );
    }
}
