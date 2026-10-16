#[path = "support/prepared_python.rs"]
mod prepared_python;

use std::path::PathBuf;
use std::process::Command;

use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_services::diagnostics::{
    durable_input_pending, execution_ledger, migration_status,
};

#[test]
fn diagnostics_store_projection_matches_python_on_python_written_rows() {
    let directory = tempfile::tempdir().expect("tmpfs fixture directory");
    let oracle = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/diagnostics/oracle.py");
    let server_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let output = Command::new(prepared_python::prepared_python(&server_source))
        .arg(oracle)
        .arg(directory.path())
        .output()
        .expect("prepared Python diagnostics oracle");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
    let connection = Connection::open(directory.path().join("canvas.sqlite3"))
        .expect("Python-written diagnostics database");
    let actual = json!({
        "executions": execution_ledger(&connection).expect("execution ledger rows"),
        "durableInputPending": durable_input_pending(&connection).expect("pending event count"),
        "migrations": migration_status(&connection).expect("migration rows"),
    });
    assert_eq!(actual, expected);
}
