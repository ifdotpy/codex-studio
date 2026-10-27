use std::fs;
use std::process::Command;

#[test]
#[allow(clippy::disallowed_methods)]
fn state_check_reads_fixture_rows_without_changing_or_disclosing_them() {
    let directory = tempfile::tempdir().expect("owned test directory");
    let root = directory.path().to_path_buf();
    fs::create_dir_all(&root).expect("temporary state directory is created");
    let database = root.join("canvas.sqlite3");
    let db = rusqlite::Connection::open(&database).expect("fixture database opens");
    db.execute_batch(include_str!("fixtures/state-check.sql"))
        .expect("fixture rows insert");
    drop(db);
    let before = fs::read(&database).expect("fixture database is readable");

    let result = Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .env("PATH", root.join("empty-path"))
        .env("CODEX_AGENTS_PYTHON", root.join("missing-python"))
        .arg("state-check")
        .arg(&root)
        .output()
        .expect("state-check command starts");
    assert!(
        result.status.success(),
        "state-check failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8(result.stdout).expect("state-check output is UTF-8");
    let report: serde_json::Value = serde_json::from_str(&output).expect("state-check prints JSON");
    let tables = report["tables"].as_array().expect("table reports exist");
    // Token-budget removal retired the budget and budget-usage reporters.
    assert_eq!(tables.len(), 44);
    let names: std::collections::BTreeSet<_> = tables
        .iter()
        .map(|table| table["table"].as_str().expect("table name is a string"))
        .collect();
    assert_eq!(names.len(), tables.len(), "one report per table");
    for retired in ["runtime_budget", "runtime_budget_usage"] {
        assert!(
            !names.contains(retired),
            "retired token-budget tables have no reports"
        );
    }
    assert!(
        tables
            .iter()
            .any(|table| table["table"] == "runtime_workspace_operations")
    );
    for name in [
        "runtime_agent_moves",
        "runtime_projects",
        "runtime_server_outbox",
    ] {
        let table = tables
            .iter()
            .find(|table| table["table"] == name)
            .expect("relocation table is checked");
        assert_eq!(table["row_count"], 0);
        assert_eq!(table["rejected_count"], 0);
    }
    let agents = tables
        .iter()
        .find(|table| table["table"] == "runtime_agents")
        .expect("agent table is checked");
    assert_eq!(agents["row_count"], 2);
    assert_eq!(agents["rejected_count"], 1);
    assert_eq!(agents["failures"][0]["pointer"], "/epoch");
    assert_eq!(agents["failures"][0]["count"], 1);
    let tasks = tables
        .iter()
        .find(|table| table["table"] == "runtime_tasks")
        .expect("task table is checked");
    assert_eq!(tasks["row_count"], 1);
    assert_eq!(tasks["rejected_count"], 0);
    let attempts = tables
        .iter()
        .find(|table| table["table"] == "runtime_execution_attempts")
        .expect("Claude attempts are checked");
    assert_eq!(attempts["row_count"], 2);
    assert_eq!(attempts["rejected_count"], 1);
    assert_eq!(attempts["failures"][0]["pointer"], "/epoch");
    for private_value in [
        "attempt-good",
        "attempt-bad",
        "valid-secret-id",
        "invalid-secret-id",
        "task-secret-id",
        "root-secret",
        "private-record-content",
    ] {
        assert!(!output.contains(private_value));
    }
    assert_eq!(
        fs::read(&database).expect("database remains readable"),
        before
    );
}
