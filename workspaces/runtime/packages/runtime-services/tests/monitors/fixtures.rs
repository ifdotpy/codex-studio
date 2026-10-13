use serde_json::Value;
use std::path::Path;
use studio_runtime_store::{AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table};

pub fn corpus(root: &Path) -> Vec<Value> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new("timeout")
        .arg("60")
        .arg(std::env::var_os("CODEX_AGENTS_PYTHON").expect("set isolated CODEX_AGENTS_PYTHON"))
        .arg("-B")
        .arg(manifest.join("tests/monitors/oracle.py"))
        .arg(manifest.join("../../apps/server/src"))
        .arg(root)
        .output()
        .expect("monitor fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("monitor fixture")
}

pub fn store(root: &Path, name: &str) -> RuntimeStore {
    let target = root.join(name);
    std::fs::create_dir(&target).expect("monitor fixture");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(root.join("template").join(database), target.join(database))
            .expect("monitor fixture");
    }
    RuntimeStore::open(Some(&target)).expect("monitor fixture")
}

pub fn seed(store: &RuntimeStore, case: &Value) {
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_events", [])?;
            tx.execute("DELETE FROM runtime_monitors", [])?;
            if let Some(events) = case["priorEvents"].as_array() {
                for e in events {
                    tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES(?,?,?,?,?,?,?,?,?)", rusqlite::params![e["id"].as_str(),e["agent"].as_str(),e["kind"].as_str(),serde_json::to_string(&e["text"]).expect("event"),e["status"].as_str(),e["created"].as_f64(),e["epoch"].as_i64(),e["turn_id"].as_str(),e["error"].as_str()])?;
                }
            }
            AgentRepository::put(tx, &AgentRecord::from_value(case["actor"].clone())?)?;
            Table::new(tx).put(&MonitorRecord::from_value(case["before"].clone())?)
        })
        .expect("monitor fixture");
}

pub fn events(store: &RuntimeStore) -> Value {
    let connection = store.read().expect("monitor fixture");
    let mut query = connection.prepare("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events ORDER BY id").expect("monitor fixture");
    let rows = query.query_map([], |r| Ok(serde_json::json!({
        "id":r.get::<_, String>(0)?, "agent":r.get::<_, String>(1)?, "kind":r.get::<_, String>(2)?,
        "text":serde_json::from_str::<Value>(&r.get::<_, String>(3)?).expect("monitor fixture"), "status":r.get::<_, String>(4)?, "created":r.get::<_, f64>(5)?, "epoch":r.get::<_, i64>(6)?, "turn_id":r.get::<_, Option<String>>(7)?, "error":r.get::<_, Option<String>>(8)?
    }))).expect("monitor fixture").collect::<Result<Vec<_>, _>>().expect("monitor fixture");
    Value::Array(rows)
}

pub fn tempdir() -> tempfile::TempDir {
    match std::env::var_os("STUDIO_MONITOR_TEST_ROOT") {
        Some(root) => tempfile::Builder::new()
            .prefix("monitor-")
            .tempdir_in(root)
            .expect("isolated monitor fixture root"),
        None => tempfile::tempdir().expect("monitor fixture"),
    }
}
