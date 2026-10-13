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
    assert_eq!(tables.len(), 20);
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
    for private_value in [
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
