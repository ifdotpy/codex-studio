use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn state_check_reads_python_written_rows_without_changing_or_disclosing_them() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "runtime-state-check-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("temporary state directory is created");
    let database = root.join("canvas.sqlite3");
    let python = std::env::var("CODEX_AGENTS_PYTHON").unwrap_or_else(|_| "python3".to_owned());
    let fixture = r#"
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.executescript('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL); CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, agent TEXT, record TEXT NOT NULL);')
db.executemany('INSERT INTO runtime_agents VALUES (?, ?)', [
    ('valid-secret-id', '{"id":"valid-secret-id","rootId":"root-secret","epoch":0,"status":"idle","sandbox":null}'),
    ('invalid-secret-id', '{"id":"invalid-secret-id","rootId":"root-secret","epoch":"broken","status":"idle","private":"private-record-content"}')
])
db.execute('INSERT INTO runtime_tasks VALUES (?, ?, ?)', ('task-secret-id', 'valid-secret-id', '{"id":"task-secret-id","agent":"valid-secret-id","reattachRecovery":{"state":"pending"},"outputPreviewOnly":true}'))
db.execute('CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
db.executemany('INSERT INTO runtime_execution_attempts VALUES (?, ?)', [('attempt-good', '{"id":"attempt-good","epoch":2}'), ('attempt-bad','{"id":"attempt-bad","epoch":"wrong"}')])
db.commit()
db.close()
"#;
    let created = Command::new(&python)
        .args(["-B", "-c", fixture])
        .arg(&database)
        .status()
        .expect("Python fixture writer starts");
    assert!(created.success(), "Python fixture writer succeeds");
    let before = fs::read(&database).expect("fixture database is readable");

    let result = Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
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
    assert_eq!(tables.len(), 36);
    assert!(
        tables
            .iter()
            .any(|table| table["table"] == "runtime_workspace_operations")
    );
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
    fs::remove_dir_all(root).expect("temporary state is removed");
}
